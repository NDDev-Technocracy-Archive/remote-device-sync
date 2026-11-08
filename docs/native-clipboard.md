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
