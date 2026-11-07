# Consented Wayland desktop

The optional Linux `rds-agent/portal` feature provides a prepared XDG portal,
PipeWire mapped-memory software capture and EI input backend. It requires
RemoteDesktop v2, ScreenCast v5, embedded cursors, keyboard/pointer grants,
monitor stream IDs and current EIS region mappings. It does not authorize an
unattended login screen, privileged scanout or hardware codecs.

The [native functional receipt](reports/rds-wayland-native-20261011.md) records
two-monitor capture/decode, restored permission and non-mutating EI sync ACKs.
Physical input/presentation, revocation and installed acceptance remain separate.

Build with PipeWire/SPA development headers and libclang installed:

```sh
cargo build --locked -p rds-agent --features portal
cargo run --locked -p rds-desktop --features portal --example wayland_check -- /absolute/private/permission.json
```

Local consent has a one-hour bound; ordinary portal RPCs have ten-second
bounds. The qualification example opens a local permission dialog, captures and decodes
frames for every selected monitor and closes its own session. It injects no
input. Optional `--verify-input-sync` checks each new lease's native EI sync ACK
with an unowned key release which the RDS hold aggregator suppresses; no keyboard
or pointer event is emitted. This is an ACK-plumbing check, not physical input or
input-to-visible-pixel acceptance. The directory must be owned by the serving user and mode 0700. State and
its lock must be ordinary, single-link, owned 0600 files; symlink components,
changed state, duplicate IDs and competing writers are refused.

The agent option `--wayland-state /absolute/private/permission.json` prepares
consent independently of binding its endpoint and starting its local manager.
Pending, failed or withdrawn screen consent disables incoming screen hosting;
the resident manager and outgoing connections keep their own lifetime. Shutdown
still cancels and joins local preparation and native cleanup. Its explicit allowlist/grant policy
still protects every remote connection. Omitting the flag does not allow a
remote caller to trigger consent. X11 serving refuses a Wayland environment,
including Xwayland's DISPLAY, instead of advertising incomplete screen access.
The shared capture/input inventory also checks the server's XWAYLAND extension
when an SSH/service environment omits session variables.

## Ownership and identity

`DesktopSource` is the backend-neutral serving seam. The agent owns the prepared
source, reads in-memory inventory and lends it to independently admitted desktop
sessions. Capture/encoding stays on the existing serving blocking worker; portal
D-Bus, PipeWire and EI native objects remain below the service API.

Pending Start retains its own request path before calling the portal and
subscribes to Response before sending. Timeout/drop cleanup closes that Request
before Session and its unique bus peer; setup failures join the close owner.
An abandoned request must not leave a stale permission dialog.

`open_with_cancel` gives local preparation an explicit cancellation future.
The agent and attended example use their retained SIGINT/SIGTERM listener during
consent, then join the pending request, session and unique bus cleanup before
returning. Cancellation also joins file work before releasing its state lock;
native readiness failures join source cleanup. Aborting the preparation task or
terminating the process forcibly cannot synchronously provide these guarantees.

Select the monitor tiles as well as permitting remote interaction. In GNOME 50,
interaction alone enables the dialog's accept button, but an empty monitor
selection can create a screen-cast session with no stream-ready signal. This is
an inference from the [dialog](https://github.com/GNOME/xdg-desktop-portal-gnome/blob/50.0/src/remotedesktopdialog.c),
[selection widget](https://github.com/GNOME/xdg-desktop-portal-gnome/blob/50.0/src/screencastwidget.c)
and [combined session](https://github.com/GNOME/xdg-desktop-portal-gnome/blob/50.0/src/remotedesktop.c)
code. RDS cannot select monitors or bypass consent on the user's behalf; an
explicit local cancellation closes a stuck request, without restarting GNOME.

One RemoteDesktop session selects input and multiple monitor sources, starts
once and connects one PipeWire remote and one EIS peer. Combined persistence is
requested only through RemoteDesktop. Start consumes the previous restore token;
the replacement is saved atomically before native setup. A withdrawn/invalid
token may require local consent again. Revocation never automatically reopens a
session in the current process.

Restored stream `id` maps to durable numeric display IDs within the permission
history. The portal guarantees this continuity for restored sessions; fresh
consent is a new source selection and requires inventory/tab revalidation.
This is not a physical monitor serial or a global hardware identity. Retired IDs remain
reserved within a bounded history. `mapping_id` matches only the current EI
region; a PipeWire node number is valid only inside the owning session. No
recycled node or different display receives an old lease. DisplayInfo.primary
marks the first selected monitor, not a compositor primary-monitor assertion.

Each prepared source admits at most eight simultaneous software encoders and
64 controlling leases; each native worker family has four process-wide slots.
Encoder permits stay with their producers until actual capture work ends.
Each monitor retains its newest owned BGRA image. Inactive subscriptions dequeue
native buffers without continuously copying full-resolution images. Declared
monitor pixels and retained image bytes are bounded separately. Padded/offset
single-plane BGRA/BGRx buffers are checked before copying; unsupported native
layouts fail closed. Native capture timestamps describe local PipeWire ingress,
not compositor scanout or physical input-to-photon timing.

## Input and shutdown

A single owned EI worker handshakes with the compositor. Keyboard input follows
the compositor's seat focus; display IDs are source/pointer routing identities,
not physical isolation of keyboard focus between monitors. Each controlling tab
gets a bounded lease; only matching display and admitted pixel geometry can
submit input. Source pixels map into the corresponding EI logical region with
its offset. Physical scale is not applied twice. Duplicate key-downs share a
physical hold and delegate repeat to the compositor. Ending one tab releases
only holds whose last RDS owner ended. It never alters another EI application's
virtual devices or GNOME's global keyboard settings.

Requests have a two-second deadline and no retry/replay. An EI sync response
establishes processing through the native protocol peer, not an application
response or painted pixels. Pause/removal invalidates old leases. Relative
pointer input is refused because this implementation scopes absolute regions.
Optional Clipboard v1 access is requested on this same session before Start.
Only a granted session advertises clipboard support; unsupported, refused or
failed clipboard setup leaves capture/input usable and never probes Xwayland.
One source-owned actor filters session signals and lends at most eight leases.
It reads changes only while a controlling reverse-clipboard lease exists, skips
its own selection ownership, retains the newest external owner and serves up to
four simultaneous selection writes. UTF-8 text uses a 1 MiB bound and two-second
nonblocking descriptor deadlines. Each transfer serial is acknowledged; source
shutdown cancels and joins descriptor work before closing the portal peer.
See [the clipboard contract](clipboard.md). Native clipboard consent and
installed cross-device behavior require separate qualification.

Worker exit guards also seal retained pixels and input readiness during Rust
unwinding, so a panicked worker cannot leave an apparently live frozen backend.
Closing the source or losing portal/PipeWire/EIS access seals input and capture,
then closes the unique portal D-Bus peer and joins its native workers under
bounded cleanup waits. Started blocking native work cannot be forcibly aborted;
its own worker permit remains held until it actually exits. Drop requests the
retained close coordinator rather than canceling it. This implementation must
still be qualified on the target compositor before claiming native acceptance.

## Checks and evidence

CI installs the required ABI headers and checks the portal agent/desktop feature
with strict Clippy. A real EI socket pair exercises two offset/fractional-scale
monitor regions, shared held keys, release, pause invalidation and a silent-peer
handshake deadline; no ambient desktop receives input. Filesystem regressions
cover token rotation, stable numbering, exclusive locks, unexpected changes,
links and private modes. Mapped-buffer and geometry checks also run portably.
Native permission, capture/decode, revoke/restart and physical input evidence is
separate from these fixtures and remains required for deployment.

Primary contracts: [RemoteDesktop](https://flatpak.github.io/xdg-desktop-portal/docs/doc-org.freedesktop.portal.RemoteDesktop.html),
[ScreenCast](https://flatpak.github.io/xdg-desktop-portal/docs/doc-org.freedesktop.portal.ScreenCast.html),
[PipeWire capture](https://docs.pipewire.org/page_tutorial5.html),
[libei sender](https://libinput.pages.freedesktop.org/libei/api/group__libei-sender.html).

## Refreshing an older clipboard permission

GNOME restores the saved clipboard grant independently of a new
RequestClipboard call. A token created before RDS requested clipboard may
therefore restore video/input while returning `clipboard_enabled=false`.
Use the explicit local agent flag `--wayland-refresh-permission` with
`--wayland-state PATH` once to request fresh consent for both monitors and
clipboard. It bypasses restoration for that preparation, retains existing state
and display history until Start succeeds, and then saves the replacement token
atomically. Cancelled preparation retains the previous state. Revalidate the
fresh display inventory before opening old monitor profiles.

Do not keep this flag in ordinary service startup: subsequent starts should
restore the newly granted token, and no remote request may trigger a new dialog.
The preparation diagnostic records only the clipboard-ready boolean, never
permission tokens or selection contents. GNOME's behavior is visible in its
[restore path](https://github.com/GNOME/xdg-desktop-portal-gnome/blob/50.0/src/remotedesktop.c).
