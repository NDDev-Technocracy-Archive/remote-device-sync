# X11 clipboard resources

An ordinary completed text read left its temporary X11 requestor window alive
until the connection closed. INCR completion and cancellation already destroyed
their requestors. Ordinary completion now does the same. The native watcher
regression compares actual X-server child windows after each short, repeated,
large and empty copy. The original runtime fails on the first short copy; the
corrected runtime returns to the original window count after every copy.

The worker also used a 250 ms poll timeout while idle. With XFixes available and
no active incoming/outgoing transfer, native events, queued jobs and close
already wake its two descriptors. That idle state now waits without a timer.
Active transfer deadlines and the legacy owner-observation fallback keep their
existing timeout. Closing still wakes and joins the actual native worker.

Qualification ran on a physical Ubuntu x64 development machine with dedicated
Xvfb servers and the normal development toolchain. The source base was
`9ffe279c315d6268d54fdc8229e20f42ef33072f`. Both snapshots include the same new
native regression/measurement fixtures. Their runtime differences are the
requestor cleanup and idle timeout above.

| Snapshot | Source archive SHA-256 | Test executable SHA-256 |
| --- | --- | --- |
| Original runtime | `b782d2766362aaa4ea6f7301304f9e693f606a0a7c47445441d7a56a32afdb83` | `a8b85bb2e512f06ca690d2aff878a02b9fff6107629cde22b7f5f253153ff1c0` |
| Corrected runtime | `ed65d046c94beb69c8e6591bc7a9614b1fabc753a844052804b954caad363b22` | `154452213cc3a267b2fd59ae704209165f835a2f13a9f8f9cccbd5c91327870c` |

One paired 20.001-second idle sample, with two native clipboard workers,
observed 163 voluntary process-thread context switches before the change and
five afterward. Each sample then published a different value, verified its
native receipt and drained both idle workers without another clipboard event.
This is a narrow idle wakeup observation, not whole-application CPU reduction,
interactive latency or a performance distribution.

The original window-count regression failed as expected with the leaked
requestor. All four native X11 cases passed with the correction: ordinary/large
INCR publication, external-owner observation without echo, bidirectional legacy
clipboard over real QUIC while media waits, and idle wake/close. Workspace
all-target denied Clippy also passed. No wire format, OS permission or native
clipboard payload limit changed. These cases require a dedicated X server and
remain explicitly ignored in ordinary display-free test runs.

The final follow-up also drains buffered native events before waiting on the
socket again. A server reply barrier deliberately queues 160 native events in
`RustConnection`; exhausting one bounded event batch reports remaining work so
the worker revisits its jobs/stop flag and drains the next batch without waiting
for another socket event. This prevents indefinite idle sleep with an existing
user-space backlog. Final snapshot
`1e11fbe3371ab2d8f2fec390486ee53fb97e45612656d926b30d60638f1386c0`
passed denied desktop all-target Clippy, all 197 display-free library cases and
all five dedicated native X11 cases. The final idle case again observed five
voluntary context switches in 20.001 seconds. No earlier failure or threshold
was removed.
