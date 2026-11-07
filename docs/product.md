# NDDev Remote

NDDev Remote is the standalone OpenNetwork product for encrypted remote screens.
It owns the complete remote-screen engine and client. RDS supplies its existing
implementation and history; NDS is an embedding consumer, not a prerequisite.

## Product responsibilities

- Device endpoint identity, explicit peer admission and bounded session ownership.
- Direct and relayed encrypted transport, path recovery and connection diagnostics.
- Native screen capture, bounded media delivery, decode and presentation.
- Authorized keyboard/pointer control and per-display session/workspace behavior.
- Native applications and reproducible desktop packages for Linux and macOS.

The first delivery preserves the working RDS wire, identities and permissions.
The native application is `nddev-remote`, with the `rds` automation CLI and
resident `rds-agent`. Existing `rds-*` library names describe the engine, not a
dependency on a separately installed legacy product.

The engine retains historical SSH, forwarding, file-transfer and audio code.
Those code paths do not enlarge a screen's permission or establish new product
acceptance. A display connection never implies an arbitrary remote shell,
file/clipboard access, a new device enrollment or a broader account scope.

## Acceptance

Capture, remote control and viewing are qualified separately on each native OS.
Linux X11 and the opt-in consented Wayland portal have implemented host paths.
The native viewer has Linux and macOS implementations. The macOS capture/input
backends are still stubs and must be implemented and qualified before macOS is
advertised as a controlled host.

A release is assessed on actual received/presented pixels, input to an owned
application, delay and recovery after network/agent changes. Active processes,
Info responses, build success and transport acknowledgements alone do not prove
this journey. A device keeps its own identity and native OS consent; migrating
the product does not authorize replacing either or copying another device's grant.

Measured interactive latency and stable recovery take priority over adding
transport knobs. Package/CI caches have independent listeners, configuration,
identities and lifetimes; cache forwarding cannot be a hidden prerequisite for
opening or restoring a remote screen.
