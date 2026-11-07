# Embedding NDDev Remote in NDS

Standalone NDDev Remote and the NDS remote module consume the same Rust engine.
An embedding adapter supplies the surrounding product's authority and lifecycle;
it does not duplicate capture, transport, codec or native input implementations.

## Ownership

| Concern | NDDev Remote | NDS adapter |
| --- | --- | --- |
| Peer connection | Endpoint/key ownership, authenticated transport and bounded sessions | Admitted device and original account/space context |
| Screen | Capture, delivery, decode, presentation and authorized input | User-facing module state and NDS presentation integration |
| Authorization | Verify the engine's explicit peer/grant and service scope | Derive that admission from Accounts; retain its purpose and lease |
| Diagnostics | Typed errors, bounded observations and tracing context | NDS telemetry delivery, correlation and privacy policy |
| Settings/state | Native endpoint and local workspace state | NDS-owned synchronization and conflict policy for its own settings |

No engine crate imports NDS Accounts, its stores, telemetry service or sync
coordinator. Standalone startup needs none of them. The executable layer owns
standalone initialization; a library call must not install a second global
telemetry subscriber or start an NDS daemon implicitly.

## Lifetime and disclosure

The adapter binds an operation to its original device, peer, scope and authority
before native work starts. Switching a user/space, revocation and expiry prevent
later admission or result disclosure under replacement authority. Cancellation
does not acknowledge completion until the engine's retained native tasks drain.
The adapter retains the corresponding purpose/key ownership until then.

Private keys, OS consent tokens and raw screen/input contents never enter
synchronized settings, presentation metadata or diagnostic payloads. A signed
reachability record, saved device name or UI selection supplies no authorization.
Restart restores desired configuration and creates a new admitted connection;
it does not replay old input, commands or a saved process-lifetime session handle.

The current NDS remote adapter is a separately qualified consumer with its own
source/dependency locks. Its read-only manager API and experimental Linux X11
receiver are not acceptance for the standalone product or for other platforms.
Adoption of a new engine revision requires the adapter's coordinated lock and
native contract checks; historical source pins and provenance stay immutable.
