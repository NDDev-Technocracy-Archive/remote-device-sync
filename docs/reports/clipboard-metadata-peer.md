# Native clipboard metadata peer

Part of [#212](https://github.com/NDDev-Technocracy-OpenNetwork/nddev-remote/issues/212).
This qualifies a shared transport/ownership component. Runtime handshake,
manager/viewer integration and actual cross-device OS pastes remain open.
The existing DesktopV5 text wire is unchanged; this component is not enabled by
that greeting. No release or installed application was replaced for these checks.

## Change

One explicitly admitted desktop owns a symmetric metadata channel, a bounded
source job set and separate requested-body budgets. Its caller supplies an
existing native provider, original authority and reduced capabilities. The
component opens no OS provider and admits only implemented text/PNG formats;
selected-file publication is still unsupported here.

`UniHello::ClipboardMetadata` appends tag 7 without changing earlier tags or
the tag-6 body layout. One framed reader owns partial metadata across event
selection. A dedicated writer applies control priority and bounded startup/write
deadlines. Neither task carries clipboard bodies or desktop input/video. Source
watching advertises metadata; it does not eagerly fetch the copied content.

Request IDs reserve their high bit for the actual desktop control-stream
direction. Without this partition, equal offer/counter values from simultaneous
copies can direct a publication failure at the wrong incoming body. A peer
capability does not choose its direction or create permission.

Dropping a pending body cancels its exact source request, including before any
body stream exists. Inability to queue cancellation seals only the clipboard
channel. Source cancellation and channel failure drain the retained native
provider; native operations still keep their original purpose through actual
completion. A blocked initial metadata read cannot hide metadata-channel
startup failure. The shared connection remains available independently.

Consuming publication preparation moves complete text/PNG validation to a
blocking owner. It shares the immutable encoded bytes and retains the exact
body credit and gesture authority if the async waiter disappears. A canceled
gesture or sticky peer refusal prevents success. Receipts are deduplicated and
distinguish verified bytes from publication; neither proves application paste.

The compiled schema-4 profile declares 16 queued metadata messages in each
direction, a 5-second startup deadline and a 2-second write deadline. The native
body and wire budgets remain separate. Missing/unknown settings, zero capacity
and earlier/unknown schemas are refused. There is no new polling timer, disk
staging, dependency or unsafe code in this component.

## Qualification

Frozen source `ee1d611fe81cfcf4f84c8411279e71cbc135d3a9e9745c1db9b4b586f97dc56f`
passed through the normal development toolchain on physical Linux and Apple
Silicon. Linux passed 42 core cases and its documentation case, denied desktop
Clippy, the isolated portal D-Bus case and 241 desktop cases (11 native-display
cases intentionally not run by that library invocation). macOS passed denied
desktop Clippy and all 208 desktop cases. Its strictly verified ordinary-debug
AppKit probe passed all 18 isolated named-pasteboard cases. Probe SHA-256:
`41f2f26a347be949f4a3a14f3a80b59921815f5183b7e8b38bf0d947fa273ab9`.

Both Iroh and Noq cases use actual endpoints, tagged streams and body framing
over explicit loopback routes. The test provider is deliberately in-memory;
these cases establish no native receiving-application acceptance. They verify:

- simultaneous multilingual text offers without a source-content read;
- requested text and PNG, validated immutable publication values and no second
  encoded-body allocation;
- stale offers, typed Control/Command peer semantics and receipt deduplication;
- cancellation of a paused source read before any body exists, followed by a
  successful request over the same metadata channel;
- a publication failure while the opposite request remains pending; a later
  body proves that the earlier failure crossed the ordered metadata stream;
- missing native metadata timing out while an independent stream still carries
  its exact payload over the same connection.

Counterexample archive
`6a6f4cee4d6ca6d392e75009ec90d82d31a84dc10611dd44d1f3aef130f720ad`
changes only the two request-direction mint/ownership methods in that frozen
source, removing the namespace partition. The unchanged opposite-request case
fails with `publication failure crossed request directions` on its first Iroh
iteration; the test process exits 101 normally. The provider, actor, ordered
metadata, payloads, timeout and assertion remain unchanged. This is not a
claimed negative run on Noq.

Final source `4c97faca5bb82e9d44ec379ed0401220e644c13e383f98bcaa77bb3dca6d62fb`
additionally observes metadata I/O failure during initial provider metadata
acquisition. Its timeout case now deliberately stalls that local acquisition.
The same final platform checks passed: Linux core tests and denied desktop
Clippy, the isolated portal D-Bus case and all 241 desktop cases (11 ignored);
macOS denied desktop Clippy, all 208 desktop cases and the strictly verified
18-case named-pasteboard probe. Final ordinary-debug probe SHA-256:
`540f6697bcfc74ba6810c89e6667aae8e6afa137e1200a23f50a146cc9d170ad`.

The ordinary-debug native probe emits the existing
`sandbox_extension_consume failed: 1` diagnostic. The successful exit does not
establish sandboxed file promises, native paste, OS permission adoption or
optimized installed-runtime acceptance.
