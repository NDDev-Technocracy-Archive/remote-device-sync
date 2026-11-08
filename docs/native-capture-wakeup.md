# Native capture wake ownership

An admitted producer shares one wake owner with its `ProducerControls` clones.
IDR and accepted-input flags retain their atomic level semantics and notify that
owner when set. Reading or consuming a flag does not manufacture another wake.
Construct controls with `ProducerControls::new`; cloning them preserves the
original flags, deadlines and wake identity together.

The serving scope owns a stop guard. Normal shutdown closes native admission and
then the wake; cancellation drops the same guard. Closing wakes both idle capture
and cadence waits. The actual blocking native worker retains its original scope,
capture/codec state and reservations until it returns. A stopped producer cannot
be rebound to a replacement session by reusing its wake.

Consented PipeWire and ScreenCaptureKit monitors retain the newest complete
native image even without a viewer. Each producer registers a weak subscription;
publication, geometry invalidation and source failure notify the original wake.
Dead subscriptions are removed without retaining closed sessions. The producer
samples a wake generation before checking image and control levels, so a change
between the check and waiting cannot be lost. It releases its sampled image
before cadence waiting and fetches the latest image afterward. An invalidated
image is never encoded from an earlier retained snapshot.

X11 owns one nonblocking, close-on-exec event FD for a producer's lifetime, alongside
its existing capture connection. A native poll waits on both FDs. Input, repair
and stop signal that original FD; the producer drains the notification and checks
the authoritative levels. Notifications before poll entry remain readable. Poll
errors end capture rather than reporting artificial damage. No FD is created per
frame, and the native poll retains its FD through actual completion.

Neither damage nor steering bypasses the configured frame interval. The cadence
wait listens only for stop; capture rereads the latest state at that deadline.
The existing one-second unchanged-picture refresh and software encoder behavior
remain. There is no periodic 10 ms idle poll, additional worker, new OS consent,
wire message or durable write in this change. Media queue/backpressure waits and
hardware encoding are separate work.

Tests exercise notification before wait entry, cadence cancellation, real Linux
event-FD readiness and lifetime, native image/failure ownership, actual H.264
producer cancellation and reservation release. The explicit ignored X11 sample
requires an owned static Xvfb display; its resource result is recorded in
[the capture report](reports/native-capture-idle.md). This fixture is not a claim
of physical display, hardware codec or whole-host CPU qualification.
