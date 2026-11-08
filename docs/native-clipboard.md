# Native clipboard

Execution: [#212](https://github.com/NDDev-Technocracy-OpenNetwork/nddev-remote/issues/212).
The existing DesktopV5 text wire is unchanged. The new native representation,
file authority and streaming core are building blocks; they do not advertise
image/file support or qualify an OS adapter by themselves.

`rds-core::clipboard` owns directional capabilities, typed limits, offers,
session/offer/request identities, exact selected-file manifests and metadata
messages. A peer can reduce admitted limits, never raise the local budget.
An unknown native body size remains unknown until its bounded stream header.
File sizes and opaque file IDs must match the previously admitted selection.
Source paths never cross the wire; debug output omits names, bodies and digests.

`rds-desktop::clipboard::stream` transfers one bounded body and BLAKE3 trailer
through generic asynchronous I/O. It rejects mismatched requests before writing
the destination and rejects truncation, corruption and trailing data. The
caller owns authorization, cancellation/deadlines, concurrency, transport
priority and the unpublished native destination. A verified receipt is distinct
from native publication and from application consumption.

`rds-sync::selection` owns the selected-file capability, independently of sync
roots and the chunk cache. Native adapters select absolute local paths or open
descriptors. Only regular files are admitted; leaf symlinks, devices and FIFOs
are refused without a blocking open. Reads use the held descriptor and explicit
offsets, with one bounded native operation per reader. Replacing the original
path cannot redirect that authority. Modification during the selected copy is
reported as a failed source. No content pre-read, chunk-cache write or second
full receiver copy is required by this core.

The desktop crate consumes this narrow file capability from `rds-sync` rather
than introducing a second filesystem-transfer implementation. Sync's configured
root authorization and resumable journal remain independent; a clipboard offer
does not grant either of those scopes.

`rds-desktop::clipboard::native` separates a native read from publication.
Text and PNG bodies are validated immutable objects. Static PNG is checked
through complete row decoding and its end marker, sharing the encoded bytes;
native TIFF conversion validates dimensions and bounds allocations before
creating pixels. The `image` and `png` packages already present in the lock are
declared directly for these operations, with only PNG/TIFF codecs enabled.

File publication uses a pinned manifest and an owned destination supplied by
the native paste operation. Native promise adapters may write directly there;
URI-based adapters must explicitly own and bound any staging. The current local
file source writes one destination and drains queued native writes before its
completion. Closing/switching/revoking a session must retain its native owners
until in-flight operations drain; no network receipt substitutes for that.

Native adapters, negotiated transport/manager channels, gesture ownership and
actual text/image/file pastes across the OS matrix remain the next acceptance
steps. The legacy text mode remains admitted during the coordinated upgrade.

## Native ownership

The serving session owns one sealed admission/drain lifetime. Capture, input
and clipboard keep that original caller context until their real native work
and OS objects are released. Normal serving completion drains these owners;
canceling the Rust future still leaves the running native guard in place.
Managed admission pins the original grant and service reservations through
that cleanup. These guards grant no new permission. A timed-out input call ends
its owning session rather than probing a replacement OS sink while the admitted
backend is still executing.

The macOS adapter shares one sleeping AppKit owner per process, a bounded queue
and body permits. A watching lease polls only native generation metadata; text
and image data are read on request. Each queued operation retains its original
scope through native completion, even when its receiver times out. No watcher
means the native owner sleeps on its queue. Consented Mac hosting uses this
owner for the existing text mode. The current viewer wrappers still require
migration to its asynchronous contract before native clipboard mode is offered.
Native files remain unsupported until NSURL authority and paste promises are
implemented; string URLs are not a substitute for native file authority.

`native_clipboard` runs against private named pasteboards. The first seven
AppKit cases passed on physical Apple Silicon hosts using the exact PR probe
artifact. The queued owner adds five separately reported cases. These probes do
not read the general clipboard, change OS grants or establish cross-device paste
acceptance. PR probe bytes are bound to their source tree and artifact digest;
release provenance is emitted only for successful main producers.

Wayland consent-state ownership explicitly unlocks its file on teardown. A
transient descriptor inherited by a concurrent spawn before CLOEXEC must not
prolong the ended owner's lock or make immediate preparation fail. A regression
holds that same open file description while the original owner ends and verifies
that the retained consent state can be reopened without a delay or retry.
