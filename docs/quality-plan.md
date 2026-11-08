# Native clipboard and resource quality

Execution parent: [#210](https://github.com/NDDev-Technocracy-OpenNetwork/nddev-remote/issues/210).
This plan extends the existing native desktop workstreams. A listed adapter or
contract is not native acceptance. Linux X11, consented Wayland and macOS each
qualify hosting, viewing and their clipboard directions independently.

## Ownership and contracts

The standalone Rust engine owns desktop media and clipboard. NDS remains an
embedding consumer. Native adapters own OS objects and their thread/lifetime
requirements; shared code owns validated representations, bounded transfer,
session authorization and cancellation. Native implementation does not use
external clipboard commands or a second resident helper.

A copy advertises representations; a permitted paste requests a representation.
Text, encoded images and selected files have distinct contracts and resource
limits. Negotiation must distinguish host and viewer capabilities in each
direction. An unavailable representation is reported without waiting for an
unrelated timeout. A file offer grants access only to its explicitly selected
files, never an arbitrary remote path or the host's filesystem.

Copy ownership remains bound to the originating session/epoch and explicit
gesture. Paste retains its destination, settings revision, focus and native
clipboard generation. Switching, revocation, closure and reconnect invalidate
unfinished work. Clipboard bodies do not enter diagnostic payloads. Native
Ctrl/Command shortcuts follow the controlled platform's semantics.

Large content uses bounded streaming with cancellation and integrity checks.
The selected-file receiver writes its destination under its native authority;
buffering and recovery do not require a second full local file copy. Bulk
traffic has an explicit budget below interactive input and media. Existing
file-transfer primitives are reused where their ownership and I/O behavior fit.

## Live diagnostics and disk

[#211](https://github.com/NDDev-Technocracy-OpenNetwork/nddev-remote/issues/211)
removes ordinary periodic full-snapshot file/log output. Current observations
and a bounded incident history live in memory and remain available through an
authenticated local read interface. Disk retains bounded incidents and
explicit, expiring diagnostic windows. Startup, failure and delivery-loss
signals remain observable. Settings, keys and consent retain their separate
durability requirements.

There is one validated observation policy, including cadence, explicit window
duration and retention bounds. Normal operation does not imply a diagnostic
capture window. A slow/full/unwritable disk cannot hold input, video or local
observation. A read receipt identifies its actual sample time and scope.

## Media and resources

[#213](https://github.com/NDDev-Technocracy-OpenNetwork/nddev-remote/issues/213)
implements actual native H.264 encode/decode behind the existing codec boundary.
The implemented software path remains available where hardware is absent.
The existing platform probe hierarchy is preserved; a present but failed
backend is distinguishable from an unavailable backend. Native input/output
surfaces stay owned until asynchronous codec work drains.

Capture age, cadence waiting, format conversion, actual codec time, network,
decode and submission are distinct measurements. Retained static-frame age is
not presented as the encoder's execution time. Prefer native surface sharing
where qualified imports support it, and account for every required CPU copy.
Wait for actual work instead of polling unchanged frames. Keep the existing
newest-frame bounds and reference-chain recovery semantics.

## Acceptance

[#212](https://github.com/NDDev-Technocracy-OpenNetwork/nddev-remote/issues/212)
qualifies text, image and file effects across the native platform matrix:
Unicode/empty/large text, image pixels and alpha, exact file integrity, native
shortcuts, remote-to-remote tabs, focus changes, reconnect, cancellation and
unsupported/oversized requests. Real native adapters and applications must be
used; transport acknowledgements alone do not prove paste.

[#214](https://github.com/NDDev-Technocracy-OpenNetwork/nddev-remote/issues/214)
records fixed static, interactive, scrolling and moving-image workloads at the
same resolution/FPS/quality before and after. Report CPU as a stated fraction
of a core, process RSS with its shared-memory limitations, attributed disk I/O,
stage latency and input-to-submitted-frame quantiles. Missing observations are
unknown. State capture cadence, retained-sample scope and measurement overhead.
File traffic, packet loss and recovery are part of the interactive acceptance.

The applicable NDDev Rust, configuration and observability requirements govern
the result: shared typed settings, bounded queues and retention, isolated native
FFI, truthful failure/loss signals, no idle diagnostic noise, and native checks
bound to exact source/configuration. Builds and native execution run on Linux
and Mac runners; source/static work stays on the source host. A release and
installed native acceptance are separate gates. Existing open path/latency
cohorts keep their thresholds and require an explained correction.
