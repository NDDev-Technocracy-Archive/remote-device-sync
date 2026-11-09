# Explicit local manager capabilities

Execution: [native clipboard #212](https://github.com/NDDev-Technocracy-OpenNetwork/nddev-remote/issues/212).
This increment prepares manager/provider integration. It does not implement the
native clipboard proxy, publish a receiving-application value or select a
different remote protocol.

Ordinary clients retain local wire version 5. `Client::capabilities()` makes one
authenticated, bounded read-only request using the explicit profile 6 and the
unchanged `List` command tag. A v5 manager can decode that command and refuse
its version before executing any work. Only its typed v5 `Version` refusal
means that the capability profile is unsupported. EOF, extra bytes, unknown
versions, wrong reply variants, capacity and native/transport errors are errors;
the probe neither retries a service command nor creates a connection or key.
The keyless `rds session capabilities --json` command exposes this result with
schema version 1 for native device/adapter qualification.

The profile-6 reply reports the existing manager instance handle and implemented
code support. `native_clipboard` remains false until its provider proxy/lease
RPC is implemented. This is neither peer authorization nor OS clipboard consent.
Profile 6 currently admits only discovery; a mutating command in that profile
is refused. Profile-5 requests, replies and selection behavior retain their
existing layouts and paths.

The integration target uses an actual same-UID Unix listener with frozen legacy
request/error bytes, malformed/refused/closed replies and a no-retry check. A
real manager with an already selected QUIC session is probed without changing
that session; a profile-6 disconnect is refused and a subsequent ordinary v5
disconnect still works. Default and owned-backend builds cover the supported
transport choices. Linux/macOS native CI run the target before merge; build-free
source checks alone are not executable evidence. The exact-head outcomes are
recorded on the pull request.
