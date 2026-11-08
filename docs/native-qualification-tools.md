# Native qualification tools

The `rds-cli` examples `native_owned_input` and `native_owned_snapshot` use an
already connected, admitted peer in the existing local manager. They never
initialize an endpoint identity, select/disconnect a manager session, request OS
permissions, or access the clipboard. Build them on native runners or devices,
with `rds-cli` features `desktop,transport-noq` as appropriate to the route.
They are qualification fixtures, not installed product commands.

`native_owned_input` reads a bounded public target ticket, matches the connected
peer, and accepts pipe commands `point`, `focus`, `F8`, and `finish`. The supplied
point is a fraction inside the explicitly admitted display's native extent.
The operator first checks the actual native pointer against its own fixture
rectangle, then clicks it and verifies focus. X11 monitor order, Portal display
IDs, and global logical/native pixel coordinates must not be assumed equivalent.
Only F8 press/release is supported; no arbitrary key macro is exposed. Expiry,
EOF and native acknowledgement failure end the fixture. A partially read pipe
command remains owned across media/event polling.

`native_owned_snapshot` receives/decode frames from one explicitly pinned peer
and display, then writes a new private PNG of the operator's selected rectangle.
It sends no keyboard or pointer events. The original decoded BGRA frame stays in
memory; row-wise conversion stores only the requested crop and never creates a
full-display PNG or a second full-image RGBA buffer. Existing output files are
refused. `--next-output` additionally stores the following decoded crop, and the
JSON result records both headers separately. It does not discard an obsolete
initial frame or replace it with a later successful image in the evidence.

The caller owns window identity, visible bounds and display mapping. For a Retina
screen, derive the crop from that owned window's native logical bounds and the
selected display's actual pixel/logical geometry. Inspect the received PNGs as
well as their metadata. An encoded frame, successful decode or transport receipt
alone is not an assertion that the screen contains the current expected state.

The normal consented Host remains the capture authority. Failure of an SSH shell's
independent native screenshot permission does not authorize changing TCC or
launching another capture identity. Existing capture defects, interrupted routes,
missing focus and failed timing cohorts remain separate retained evidence.
