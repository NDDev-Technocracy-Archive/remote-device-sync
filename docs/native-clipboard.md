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
`receive_file` uses that same framing/integrity path with the owned native file
destination. It accepts only a selected-file request, validates the complete
header before filesystem work, refuses a nonempty/nonregular destination and
moves one reusable body chunk into each positional native write. The bounded
duplex/native-file case passed on Linux and macOS without a staging copy.
`receive_reader` retains a remote selected file as a bounded reader and a
completion proof. Native destinations and remote-to-remote forwarding consume
that capability directly, without a local file, pump task or second chunk
queue. The generic receiver and remote reader share one incremental
length/trailer/EOF verifier. A corrupted upstream proof prevents a successful
downstream trailer even after all body bytes were forwarded. Linux/macOS cases
verify both the actual destination bytes and this failed-completion behavior.

`rds-sync::selection` owns the selected-file capability, independently of sync
roots and the chunk cache. Native adapters select absolute local paths or open
descriptors. Only regular files are admitted; leaf symlinks, devices and FIFOs
are refused without a blocking open. Reads use the held descriptor and explicit
offsets, with one bounded native operation per reader. Replacing the original
path cannot redirect that authority. Modification during the selected copy is
reported as a failed source. No content pre-read, chunk-cache write or second
full receiver copy is required by this core.

Every selection constructor requires the physical owner's `DescriptorBudget`.
Path selection reserves before open; imported native handles reserve before
metadata access. A held descriptor drops its FD before its reservation on both
normal completion and admission errors. Readers and canceled native operations
retain the descriptor and its budget until actual I/O returns. A new offer,
closed provider or replacement owner cannot reset capacity still used by a file.

The native policy is versioned in
`crates/rds-desktop/config/native-clipboard.json`. It declares every setting,
including nested representation limits; absent/unknown fields and unsupported
schema versions are refused. `Policy::shipped` reads this compiled common
profile without filesystem/environment access; an embedding may supply its own
validated typed policy. Native selected descriptors, queued jobs, native body
operations and directional wire transfers are separate allowances. The
standalone profile admits 64 retained selection FDs process-wide, independently
of the protocol's 128 files per manifest. A capacity error never silently admits
only part of a selected copy.

Schema 2 adds mandatory X11 selection settings: independent native
selection-transfer count and chunk size, transfer/timestamp deadlines,
legacy owner-observation interval and bounded event batch. They are separate
from native jobs and wire body concurrency. Unsupported schema versions and
missing/unknown backend settings are refused rather than filled with defaults.
Schema 3 additionally declares the portal FD chunk size, independently of the
wire and X11 chunk settings. Its common profile remains compiled and strict.

Native viewing and serving providers share the same weak process budget pool.
Each selected descriptor retains that exact physical owner, independently of a
pasteboard worker's lifetime. A replacement with a different limit is refused
while an old descriptor/native owner remains; an explicit new policy applies
after the old resource drains. The source/reader tests exercise this boundary
and capacity rejection before even a nonexistent pathname is opened.
Native checks and source/probe bindings are recorded in
[the descriptor admission report](reports/native-clipboard-descriptors.md).

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
The native `Destination` binds its opened file to the original transfer/paste
authority before filesystem work. Each positional write moves the one bounded
chunk into a blocking owner, and returns that same buffer after completion.
Canceling its waiter retains the descriptor and original caller through the
running syscall. No queued Tokio file write or second write-buffer copy can
outlive that purpose; the caller still owns unpublished-destination cleanup.
The requested body's reservation is added to that original destination owner,
never substituted for it. A canceled wait cannot free a body slot while its
native write still runs; the actual syscall retains the complete owner pair.

Native adapters, negotiated transport/manager channels, gesture ownership and
actual text/image/file pastes across the OS matrix remain the next acceptance
steps. The legacy text mode remains admitted during the coordinated upgrade.

The requested-body transport now uses the existing connection's tagged router,
one isolated uni stream per request and independent bounded directional budgets.
It claims before request metadata is sent and resets canceled partial sends.
Outgoing selected-file admission reserves a body slot before native open, and
passes that original reservation into every actual source syscall and final
version check. Native readers retain it after their async sender is canceled.
Representation responses use the same pre-read admission: `Streams::response`
reserves before a native text, image or manifest read. The one-shot response
passes `original()` into that read and consumes the admitted slot when sending
the validated bytes. Incoming verified bytes retain their reservation through
parsing and native publication; publication receives that same original owner.
Canceling either waiter cannot free capacity while its native job still runs.
Selected file capabilities keep their separate selection authority, rather than
retaining a manifest body's transfer credit for the lifetime of a file promise.
A forwarding source adds that owner to its upstream reader/proof; it never
replaces the upstream or native paste authority.
The body owner also owns one metadata queue. Request ID allocation, exact route
claim and queue commit share a synchronous admission section; queue rejection
precedes body/ID allocation. Each request retains its original purpose before
publication, and that cleanup precedes body-credit release. Selected-file
requests, receipts and cancellations use this same queue. See
[request-order qualification](reports/clipboard-request-order.md).
It neither opens an OS provider nor authorizes clipboard access by itself.
An admitted request also owns its exact peer-failure slot before metadata is
published. `Streams::refused` validates the desktop and routes a sticky first
failure only to that request. It interrupts body admission, reads, native-file
receives and remote file completion without closing the screen connection.
Verified representation bytes retain that signal and their original body
reservation through parsing/publication. Unknown or finished IDs create no new
authorization. The original native work still owns its reservation until actual
cleanup. See [refusal qualification](reports/clipboard-refusal.md).
Real loopback tests cover simultaneous requests, a late canceled response,
selected-file integrity without a staging file, and progressing control/media
beside a backpressured file. The native handshake, manager and GUI still need
coordinated integration before this mode is advertised to a user.

`native::RemoteFiles` binds one validated offered selection to its negotiated
body owner and metadata queue. Its request carries only an opaque selected ID
and the manifest's exact length. Native file promises write straight into the
receiving application's destination through the shared verifier; forwarding
instead exposes the same bounded reader/proof with no staging. A verified body
receipt does not claim OS publication. Explicit selection/paste-scope revocation
seals new requests and interrupts existing reads. Merely changing the clipboard
must not cancel a copy already admitted to its original destination. Both reader
and proof retain cancellation ownership until released; incomplete work sends a
bounded cancellation notice.
This capability does not publish a clipboard, admit an unnegotiated peer, open a
sync root or provide the still-required native promise/URI adapters.

## Native ownership

The serving session owns one sealed admission/drain lifetime. Capture, input
and clipboard keep that original caller context until their real native work
and OS objects are released. Normal serving completion drains these owners;
canceling the Rust future still leaves the running native guard in place.
Managed admission pins the original grant and service reservations through
that cleanup. These guards grant no new permission. A timed-out input call ends
its owning session rather than probing a replacement OS sink while the admitted
backend is still executing.

Native queued jobs release the operation's original purpose before returning
body credit and acknowledging drain. Real Mac/X11 reads hold that destruction
open and verify that close stays pending; restoring the earlier job order makes
the same checks fail. See [cleanup qualification](reports/native-cleanup-order.md).

The macOS adapter shares one sleeping AppKit owner per process, a bounded queue
and body permits. A watching lease polls only native generation metadata; text
and image data are read on request. Each queued operation retains its original
scope through native completion, even when its receiver times out. No watcher
means the native owner sleeps on its queue. Consented Mac hosting uses this
owner for the existing text mode. The current viewer wrappers still require
migration to its asynchronous contract before native clipboard mode is offered.
The prepared `DesktopSource` exposes an in-memory native-provider lease factory:
the caller supplies its original admitted clipboard context before jobs begin.
It does not open OS consent or fall back to an unrelated backend. Provider close
seals new work and drains the retained native operations; offered file handles
must be released before awaiting that boundary.
File sources are read as native NSURL objects, with their resource scopes and
original caller held by every selected descriptor and reader. Foundation's
filesystem representation is bounded across the whole selection; a serialized
string URL never substitutes for the native object. Unscoped local URLs still
require ordinary process filesystem access. Successful resource-scope starts
are balanced after descriptor cleanup. Native file publication/paste promises
remain unsupported, so macOS source and receiver capabilities differ.

`native_clipboard` runs against private named pasteboards. The first seven
AppKit cases passed on physical Apple Silicon hosts using the exact PR probe
artifact. The queued owner covers native publication, cancellation and the
text-wire adapter; the native file case reads an
explicit NSURL selection after its pathname has been moved/replaced. Its held
descriptor supplies the original bytes while the caller authority survives
through completion. These probes do
not read the general clipboard, change OS grants or establish cross-device paste
acceptance. PR probe bytes are bound to their source tree and artifact digest;
release provenance is emitted only for successful main producers.

The text-wire adapter is shared by macOS and the consented portal provider.
Its current metadata/read future belongs to the adapter, so cancellation of
`next_change` by a control-input select cannot discard an admitted read or
advance past its undelivered body. Close seals its owner and drops that waiter;
the provider still drains the actual native job under its retained original.
An explicit publication supersedes an undelivered read result. The AppKit owner
refreshes a differing idle metadata cache before reporting a changed generation.

Native API references: [NSPasteboard object reading](https://developer.apple.com/documentation/appkit/nspasteboard)
and [NSURL resource scope](https://developer.apple.com/documentation/foundation/nsurl/startaccessingsecurityscopedresource()).
These APIs do not establish sandboxed or receiving-application acceptance by
themselves; those remain separate native platform cases.

Wayland consent-state ownership explicitly unlocks its file on teardown. A
transient descriptor inherited by a concurrent spawn before CLOEXEC must not
prolong the ended owner's lock or make immediate preparation fail. A regression
holds that same open file description while the original owner ends and verifies
that the retained consent state can be reopened without a delay or retry.

## Prepared X11 provider

`native::prepare_x11` opens the explicitly selected X server on a sleeping
native worker under its original physical application/backend owner. Clones
share that owner; a different owner or policy cannot replace it while it is
alive. The prepared factory creates in-memory per-session provider leases.
Lease close seals and drains that session's actual native calls; physical close
also drains the board/windows before releasing process admission. This API is
not a Wayland fallback and does not open screen-capture consent.

The provider supports UTF-8 text and static PNG through the same ICCCM
TARGETS/TIMESTAMP/INCR edge as legacy text. XFixes generation notifications wake
metadata watchers; content is read only for an admitted representation request.
A reply barrier observes preceding ownership changes before snapshot/read/
publication. Queued and running native operations retain their original body
and purpose even after their async waiter is canceled. PNG publication and
owned reads share immutable bytes rather than re-encoding or staging them.

Native selection/property limits and deadlines come from the common profile.
The event loop drains buffered events before polling and sleeps until a real
native/job event or the nearest active transfer deadline. The new provider
requires XFixes; the existing legacy observation fallback remains a distinct
supported path. File reception/source URIs, viewer providers, manager/GUI
integration and negotiated native mode remain unfinished and are not advertised
by this increment.

## Consented Wayland provider

The existing combined RemoteDesktop session supplies the portal provider.
Clipboard signal streams are prepared before Start and activated only after
the portal grants clipboard access. Creating a per-session provider is an
in-memory operation under the caller's admitted original context; it cannot
request another consent session or fall back to X11. This is a hosting edge,
not a screen-capture permission requirement for a clipboard-only viewer.

Owner notifications carry bounded text/PNG type metadata. Bodies are read only
on request through nonblocking owned descriptors. Published and received PNG
bytes remain immutable; there is no payload cache or staging file. A process
budget shared across physical portal replacements bounds native body work.
Canceled waiters retain each queued or running operation's original context;
descriptor cleanup precedes purpose, body-credit and drain acknowledgement.
The owner sleeps on native signals, jobs and active transfers, without the
old periodic cleanup tick.

Read completion checks the observed native generation. Publication checks the
expected observed generation before SetSelection; the portal has no atomic
cross-client compare-and-set operation. Its method reply acknowledges native
publication, not consumption by another application. File formats are absent
from this provider's capabilities. Native host/manager/viewer negotiation and
real application pastes on the desktop matrix remain separate acceptance steps.
