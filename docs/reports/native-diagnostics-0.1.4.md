# Native diagnostic storage qualification

Date: 2026-11-08. Scope: [#211](https://github.com/NDDev-Technocracy-OpenNetwork/nddev-remote/issues/211)
within [#210](https://github.com/NDDev-Technocracy-OpenNetwork/nddev-remote/issues/210).
Clipboard, hardware codec and mixed-load acceptance remain independent work.

## Source and binaries

Implementation/fixture head: `a0cc41cc5b011cd464a43611c923cab7cf66347c`.
Tree: `02d240a2bcc6db8adade1fa64b8b5da0612088c3`.
Native CI producer: [42447735981](https://github.com/NDDev-Technocracy-OpenNetwork/nddev-remote/actions/runs/42447735981).
Its PR merge revision `710e8b7a459f7664c09bb94347cfbc14e0e507f8`
has exactly that tree, with parents `291a4720a065b2a1c58c43180c7b49277d200805`
and the implementation head. These are qualification artifacts, not published
release or managed device adoption evidence.

| Executable | SHA-256 |
| --- | --- |
| Released Linux viewer 0.1.3 baseline | `43c729ecba774a0da1a54f544104e37a3c2c8cd68b341c0b8061545036ea0d81` |
| Linux release candidate, built on native qualification PC | `cde0c09b47b66e91eac6612b2981ff194e896346d060a76ba64f0422d76383ad` |
| Mac viewer, native CI | `9b5378df50ae9f7e27d3bab0851afc6385e5e21984a43ed2f7290e26ffaf869b` |
| Mac local observation CLI, native CI | `285c59b5fbdecec20cf3c3771836ffec07030b8f688164e38fd8a73977c993ea` |
| Mac synthetic qualification fixture, native CI | `9f3cc45db5cbc469d05bcd09a6b31bf5a6caac19d508e6fcd2b7ac23297a8f77` |

## Workload and results

Each native product viewer consumed synthetic 640×360 H.264 over loopback Noq
through the real local session manager. The fixture's explicit server-only mode
kept preview windows out of the measured journey. Clipboard was disabled;
payload receipts were enabled. Each viewer used an isolated workspace/config,
a default zero-length file capture window, a warm-up after connection, then a
20-second sample. No OS capture, hardware codec, WAN or optical latency claim
follows from this workload.

| Native presentation environment | Viewer | Periodic snapshot rewrites in interval | Attributed process disk bytes written | Submitted frames in interval |
| --- | --- | ---: | ---: | ---: |
| Physical GNOME Wayland session | released 0.1.3 | 10 | 65,536 | 599 |
| Same GNOME Wayland session | candidate 0.1.4 | 0 | 0 | 599 |
| Mac M1 GUI session | candidate 0.1.4 | 0 | 0 | 577 |
| Mac M2 GUI session | candidate 0.1.4 | 0 | 0 | 579 |

Linux accounting used `/proc/<pid>/io:write_bytes`. Mac accounting used the
native `proc_pid_rusage` v2 `ri_diskio_byteswritten` counter. These are process
storage counters; they do not claim to account for writes attributed to another
OS service. Startup, final workspace persistence and explicit reports lie
outside the sample. GPU/codec execution and CPU reduction were not measured.

All candidate read interfaces returned the scoped current sample from memory,
reported availability and zero query/write/admission losses, and left no
periodic state or incident files in this healthy workload. Diagnostic directory
length did not constrain the independent runtime socket. Existing managed
desktop hosts and the PC workspace retained their original PIDs.

## Failure and boundary evidence

The first native fixture exposed an overlong Unix socket address when IPC was
placed under the retained diagnostics path. The correction uses the existing
checked same-UID socket boundary in the per-user runtime namespace. Unsupported
addresses fail before creating directories. Linux and Mac native tests cover
versioned queries, missing/closed scopes and exact lifetime cleanup.

Disk-failure, bounded queue/loss accounting, exact-inode cleanup and expiring
file-window cases passed the native CLI test lanes. The final native Linux
follow-up comprised 39 desktop CLI library cases, 10 binary cases and 22 client
cases, plus warning-denied Clippy; the public producer also ran the native Mac
adapter/CLI lane. The original preview mode keeps its full multi-display
opening/production/closing checks; external-viewer mode separately verifies
that every actually opened synthetic display closed without held input.

Xvfb/software presentation stalled on the released baseline as well as the
candidate, while decoding continued. A separate X11 viewer run under physical
GNOME XWayland submitted 102 baseline and 49 candidate frames during a
20-second interval with about 599 received frames. The candidate failed the
100-frame progress threshold. Its coarse draw-path stage cannot distinguish
surface acquisition, upload and submission waits. This cohort remains open in
[#214](https://github.com/NDDev-Technocracy-OpenNetwork/nddev-remote/issues/214);
there is no X11 smoothness acceptance or explained renderer fix in this change.
The independent healthy Wayland/Mac disk result does not close that cohort.

Physical restart, native host replacement, hardware codec and cross-device
clipboard application effects are outside this diagnostic qualification.
