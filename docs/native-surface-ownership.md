# Native surface ownership

The physical X11 minimize/restore comparison exposed approximately one-second
`get_current_texture` calls on the UI thread. The decode-demand increment keeps
its failed recovery gate open. A resume-time reconfiguration did not fix it and
was removed. The driver/WSI cause and older unlabelled failures remain unresolved.

Surface configuration and acquisition now run as one bounded operation outside
the native event loop. Each window retains at most one operation/result. A
process-wide reservation uses the existing eight-display limit and stays in the
actual native closure until it returns. Canceling a waiting window aborts queued
work; running work retains its original surface, window and device. No replacement
window can escape that native-work bound. No polling thread or payload queue is
added.

The operation carries no frame, texture upload, UI command or native clipboard
data. On completion the UI takes its newest admitted image. A replaced session's
acquisition is retired separately; its wall time cannot be attributed to the new
session. Hidden windows admit no new operation and discard a returned native image.
Resizes keep only the newest size and discard an old-size image before configuring.
Configuration never overlaps a live acquired image or drawing on that surface.

Upload, command encoding, submission, `pre_present_notify` and presentation keep
their existing event-loop ownership. This preserves Wayland's ordering of surface
state before presentation. UI texture deltas remain retained until submission.
In the multi-window mode, completion wakes only windows with pending session work
or a returned acquisition; one window cannot start an idle acquire loop in another.

`render_stage = "waiting for surface"` and its age include worker admission,
scheduling and native waits. Returned configuration/acquisition timings measure
the native calls themselves. `surface_waits` counts asynchronous waits separately
from native API skips. `retired_surface_acquisitions` counts discarded operations;
neither counter proves that a frame was presented. Visible submission debt and
the existing fault thresholds remain active while a surface is slow.

The [wgpu 30 surface contract](https://docs.rs/wgpu/30.0.1/wgpu/struct.Surface.html)
defines cross-thread ownership and prohibits reconfiguration with a live acquired
image. Its [presentation contract](https://docs.rs/wgpu/30.0.1/wgpu/struct.Queue.html#method.present)
defines Wayland's pre-presentation ordering. These APIs are used through their
safe Rust types; the product adds no platform FFI or driver setting.

Native visible/hidden recovery, idle/multi-window resource use, input responsiveness,
resize/closure and actual source-bound Linux/macOS presentation remain acceptance
requirements. This implementation alone is no FPS or latency improvement claim.

The [physical visibility comparison](reports/native-visibility-recovery.md)
records Linux X11 and packaged M1/M2 hidden-load and recovery results, their exact
builds and the independent failed/unqualified cohorts which remain open.
