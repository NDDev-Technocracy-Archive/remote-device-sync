# Local device connections and session ownership

Status: manager enabled by default for connectivity commands, **W2.4 partial**.
This does not close W2, W5 or W6. See the [initial Linux receipt](reports/rds-local-sessions-20260925.md)
and [ownership/migration receipt](reports/rds-identity-migration-20260925.md).

`rds session capabilities --json` reads the local manager's explicit capability
profile without changing connections or reading identity material. It distinguishes
an unsupported discovery profile from a supported interface whose native clipboard
proxy is still unimplemented. It does not report OS consent or end-to-end clipboard
acceptance; see [the protocol and qualification scope](reports/local-capabilities.md).

## Ownership and use

`rds-agent` hosts outgoing sessions
beside inbound service. Both use clones of the **same bound endpoint**, identity,
relay registration and metrics registry. The manager never reads a key or binds
another UDP endpoint. The agent supervises and closes both services. It holds
the [exclusive persisted-key owner](identity-storage.md#runtime-ownership)
through shutdown, including with the explicit server-only `--no-control` flag.

`rds-client` owns outgoing protocol operations and the local manager/client.
It depends on core/net/discovery/observe and the existing rds-sync engine. Agent and CLI depend on it; the former
`rds-cli` library exports remain compatibility re-exports. The manager uses
existing Tokio, rustix, postcard, BLAKE3 and random-number libraries.
The later [native SSH increment](ssh.md) adds a standard protocol library at
the CLI stream boundary, without changing manager endpoint ownership.

The default is `<key-file>.control` beside the agent key. CLI derives it from
the default XDG_CONFIG_HOME/HOME key path, without reading/creating a key.
An agent with a custom key/control path needs the corresponding CLI
`--control-dir <private-absolute-directory>`. Missing directory components are
created with mode 0700 by the agent only, after validating each parent. Components must be
real directories owned by this user or root, without other-user write access.
A root-owned sticky ancestor such as `/tmp` is allowed. On macOS use canonical
paths, such as `/private/tmp` instead of the `/tmp` symlink.

```sh
# Merge the control flag into the local agent's existing configuration.
rds-agent --control-dir /absolute/private/rds-control <agent-options>

rds session --control-dir /absolute/private/rds-control connect <peer-ticket>
rds session --control-dir /absolute/private/rds-control connect <second-ticket>
rds session --control-dir /absolute/private/rds-control list
rds session --control-dir /absolute/private/rds-control use <session-id>
rds session --control-dir /absolute/private/rds-control ping
rds session --control-dir /absolute/private/rds-control info
rds session --control-dir /absolute/private/rds-control ssh --user <account> --host-key /private/device-host.pub --identity /private/operator-key
rds session --control-dir /absolute/private/rds-control send ./file.bin
rds session --control-dir /absolute/private/rds-control recv file.bin --dir ./downloads
rds session --control-dir /absolute/private/rds-control disconnect <session-id>
```

With the default path, omit every `--control-dir` above. Ordinary
`rds ticket`, `rds ping <target>`, `rds info <target>`, `rds ssh <target>` and
`rds forward <target>`, `rds send <target> <file>` and
`rds recv <target> <relative-path> --dir <directory>` use the same manager. Target commands connect/reuse
a session, then pin its handle; they never disconnect another CLI's
operation on exit. The session remains visible in `session list` until explicit
disconnect, peer loss or agent restart. `ticket` returns current addresses
immediately and does not await relay readiness. It is still an address snapshot,
not proof of reachability. `rds id` remains an offline key reader/creator.

There is **no direct fallback** when the manager is missing or rejects a request.
Transport, key and directory options belong on the agent; passing them to managed
commands returns an actionable error instead of silently ignoring them. The
global `--grant` remains supported on target commands. `session connect` uses
`--grant-file`. Both cap the file at 64 KiB and leave remote authorization to
the existing admission protocol. Grant inputs must be regular files; final
symlinks and special files are refused using nonblocking, no-follow opens, so
a FIFO cannot hang startup. The CLI uses the existing rustix library directly
for this boundary; no package/version or external helper was added.

Compatibility: `rds --direct --key-file <separate-key> ...` explicitly binds
an independent endpoint and acquires the same exclusive key owner as the agent.
An occupied key is an error. Direct mode requires a persisted key path; there is
no accidental ephemeral-identity fallback.
Send/receive default to the manager, and `desktop` now uses it too — the
manager owns the remote session and relays encoded frames while the CLI
decodes locally. `desktop --direct` remains for native/direct use.
The local wire version is now **5** (adds the managed desktop channel);
upgrade CLI and agent together. Old/new local versions fail without
mutating session state.
Remote ALPN is unchanged, with an appended isolated sync greeting; signed grant v3 accepts v2 payloads but a
pinned tenant/policy binding refuses them. See [renewal contract](grant-leases.md).

`list --json` returns instance ID, generation, endpoint, selected handle and
entries. Handles print as 32 hexadecimal characters. `ping`, `info`, `ssh`,
`forward`, `send` and `recv` accept `--session <id>`. `connect --grant-file <path>` uses a grant
issued to **the local agent identity**; file and resulting frame must each fit
64 KiB. Session commands dispatch before identity/config loading and reject
direct endpoint/directory/grant options rather than silently ignoring them.

Tickets and pinned keys use the existing resolver. Agent `--directory` enables
signed-record lookup. Names additionally require `--registry-key`; optional
`--registry-epoch`, `--registry-state` and repeated `--registry-rotation` provide
durable trust/rotation. The trust-store default is beside the endpoint key.
Registry trust is distinct from incoming revocation authority. It loads on a
blocking worker before endpoint bind. The manager inherits the verified
resolver; unsigned client-supplied name-to-key mappings are not accepted.

SSH now uses an [embedded Rust client](ssh.md) over OpenTcp, with an explicitly
trusted host public key and standard remote PTY requests. It opens no local
TCP listener. Existing SSH/TCP port forwarding remains `forward`; its listeners
reject non-loopback addresses. Other local users can reach a forwarded TCP port:
Unix control-socket UID checks do not authenticate those TCP clients.

Saved viewer device profiles may include up to four distinct unicast
`local_addresses` with nonzero ports. `ConnectPreferred` is an additive IPC v5
command and requires a ticket or pinned endpoint key. The manager reserves the
peer before dialing, attempts the direct candidates within one shared second,
and falls back to ordinary verified resolution of the same identity on dial
failure. QUIC authenticates the pinned key; a successful dial followed by service
or grant refusal is final and never replays admission through another route.
Transport cache/discovery can supplement candidates; latency path policy still
selects among established paths. No ping result or IP address establishes trust.
After a failed preferred attempt, the manager awaits removal of only those
Iroh IP hints without a live authenticated path before resolving the fallback.
Otherwise a wrong-key address can race the relay again and fail its handshake.
Cache cleanup has a separate one-second bound; a failure to finish it stops the
connection rather than dialing with uncertain cleanup. Live authenticated
paths, other peers and relay/custom addresses remain intact. Earlier Open or
Inactive history alone cannot protect an address now belonging to another key;
ready connection closures are observed before deciding which paths to preserve. Owned
Noq has no shared caller-hint cache and needs no corresponding removal.
Older managers refuse this command; empty hints retain the existing Connect
command. All monitor reconnections and saved-profile inspections retain hints.

## Consistency and lifetime

- At most 32 sessions, including pending dials. Random 128-bit handles are
  process-lifetime values. Restart loses ephemeral sessions/selection, changes
  the manager instance ID and never restores a remote command.
- Snapshot generation changes on transitions. Short state transactions never
  hold a lock across resolution, dialing or service I/O.
- One session per peer. Repeated connect with the same credential fingerprint
  reuses a connected session without another dial/Authz. A same-credential
  pending duplicate joins the original bounded dial, waiting until its owning
  response publishes readiness. Canceling a waiter does not cancel that owner;
  canceling/failing the owner or explicitly disconnecting wakes waiters with
  an unavailable-session result. Different credentials still conflict; renewal
  remains serialized. A pinned key/ticket reuses live/pending state before another
  directory lookup, while names still require verified resolution. The table
  stores a digest, not the grant payload. Requests/grants/upstream errors are
  not logged or used as metric labels.
- `session renew --session <id> --grant-file <path>` preserves the connection and
  its existing streams with a same-scope signed extension. It serializes with
  connect/reuse; a successful response changes the stored credential digest.
  Cancellation/failure during the reserved transaction removes only that session.
  Non-streaming replies require EOF after state publication; trailing bytes fail.
  There is no issuer fetch/automatic renewal loop yet.
- Remote Authz validates managed admission; plain allowlist mode uses one Ping
  reply to establish readiness. TCP-only grants need no Ping permission. Existing
  remote scope, expiry and revocation enforcement continues to apply.
  Snapshot/connection readiness is published after the owning reply is written;
  an authenticated association awaiting publication still appears Connecting.
- The first successful connection is selected when selection is empty. `use`
  changes defaults for new operations; existing streams keep their peer. A
  forward pins a handle before listening, including for later accepted sockets.
- Disconnect closes that peer and its streams or cancels its pending dial. Peer
  loss clears a selected handle; no other device is silently substituted. The
  forward checks its pinned session once per second and exits after its loss or
  manager loss. A stalled control request can extend detection to its deadline.
- Dropping an IPC connect caller cancels the operation and reservation. Failed
  reply writes roll back new sessions. If a reply was fully written but receipt
  is uncertain, `list` can show the completed connection and repeated connect
  reuses it. This is not exactly-once RPC or automatic remote-command replay.
- Shutdown seals admission, closes sessions and joins workers. Dropping the
  server aborts owned tasks. Manager shutdown leaves a retained shared endpoint
  usable. Transport closure is reconciled before state operations and on the
  sampling tick. Removed sessions are not a durable history.

Selection is currently CLI state. Desktop focus and pressed-input release
remain viewer work; this manager does not claim those semantics.

## Local access and budgets

`control.sock` has mode 0600 in the private directory. Both endpoints compare OS
peer credentials to effective UID using Tokio's Linux/macOS adapter. The
dedicated directory is exclusively locked for the server lifetime. Regular
files/symlinks are refused. A dead owned socket is recovered only under that
lock, after a refused connect probe and inode recheck. Live listeners are not
unlinked. Cleanup removes only the socket inode created by this owner. Same-UID
processes are within the trust boundary; this is not a multi-user privileged
broker.

| Resource | Bound |
|---|---|
| Sessions, including pending | 32 |
| IPC workers | 96; acceptance pauses at capacity |
| Long-lived TCP streams | 64; leaves control worker space |
| File transfers | 8 agent-wide; 1 per outgoing session; immediate capacity/busy refusal |
| Managed desktop channels | one shared stream-permit slot (of 64) per channel; encoded payloads ≤ 32 MiB each, forwarded in arrival order |
| Request prelude / reply write | 5 seconds each |
| Resolve + dial + Authz + operation | 45 seconds total |
| Client exchange, including connect/response and control EOF | 55 seconds for short operations |
| File transfer | 1 hour absolute engine budget, 300-second protocol stall bound; 45-second IPC allowance plus 10-second client margin |
| Local/relative sync path text | 8192 bytes each, UTF-8 |
| Framed postcard control message | 64 KiB; trailing payload rejected |
| Target text | 8192 bytes |
| CLI forwarding workers | positive 16-bit limit; default 64 |

Wire types live in `rds-core::local`, with explicit request/response versions.
Local IPC is **version 5**; upgrade CLI and agent together. Remote ALPN stays
`rds/0`, with the appended `SyncTransfer` greeting described below. There are no unbounded task/command queues; the OS
backlog is separate. TCP bodies do not inherit the prelude deadline and use the
existing reset/stop cancellation guard. Operations never automatically retry a
remote side effect.

Local TCP bodies use explicit bounded `Data` / `Finish` frames. A direction FIN
never half-closes the Unix socket, so caller departure remains unambiguous while
a remote TCP service is silent. The Rust AsyncRead/AsyncWrite adapter owns one
pump and a 16 KiB buffer per direction. Drop aborts its pump and closes IPC;
explicit close joins it. Write shutdown waits until queued upload bytes and its
Finish frame have reached IPC. Both clean FINs release QUIC streams normally,
preserving pending data; cancellation/errors reset them. Empty/oversized body
chunks and data after upload FIN terminate that body. The remote TCP byte stream
and remote RDS protocol remain unchanged. No OS hangup heuristic is required.

Authenticated admin scrapes add aggregate
`rds_agent_local_manager_{snapshot_available,connected,connecting,capacity}`.
The weak observer uses `try_lock`, marks unavailable explicitly and retains no
network resources. Outgoing samplers feed existing shared network counters.
The Vector admin allowlist already accepts `rds_agent_`; no pipeline change or
production deployment is implied.

## Managed single-file transfers

Both target commands and `rds session send/recv [--session <id>]` reuse the
agent identity. Selection is resolved once; an existing transfer cannot move to
another device. Paths are resolved to absolute paths **in the CLI process**,
then explicitly delegated to the authenticated same-UID agent. This is not a
privileged file broker: agents and their CLI must use the same user/filesystem
view. Local paths must be UTF-8; source opens reject non-regular files without
blocking on FIFOs. Remote paths, manifests, journal locks, verification and
atomic destination replacement retain the sync engine's confinement rules.

Every operation uses a fresh random 128-bit ID in the appended remote
`StreamHello::SyncTransfer` and `UniHello::SyncTransfer` variants. Older agents
reject the unknown greeting before file I/O; there is no automatic downgrade to
legacy service-kind routing. Legacy explicit-direct transfer remains compatible.
The uni router allows 32 live routes and removes dropped inboxes; historical
transfer IDs do not grow the map. Delayed canceled tags cannot enter a new
transfer's inbox. One active sync service per remote connection is still the
admission policy; multiplexed concurrent files are not claimed.

Success requires the verified `Done` exchange and clean control completion.
Closing IPC or pressing Ctrl-C resets only this transfer's control streams;
its uni inbox and chunk task group are dropped. Other TCP/SSH streams and the
manager session remain usable. Blocking filesystem calls already in progress
cannot be revoked: cancellation, timeout or a lost reply can leave a completed
commit. Inspect the destination before retrying; no automatic replay or rollback
is promised. An immediate retry may encounter the remote service's transient
busy refusal while cancellation cleanup finishes. A remote stream RESET is
not an acknowledgement that the independent local manager has finished
dropping its admission permit; cancellation fixtures bound that local release
without replaying an accepted transfer.

`sync_send` / `sync_recv` operation telemetry records duration and
`ok` / `error` / `cancelled` outcomes through the existing safe JSON and Vector
allowlists. No path, transfer ID, credential or file content is exported. The
agent records local operation completion; it does not claim WAN SLOs or an
end-to-end receipt after the CLI disappears.

See the [managed-transfer receipt](reports/rds-managed-sync-20260926.md).

## Managed desktop channels

`rds desktop <peer>` (and `rds session desktop --session <id>`) default to the
manager like send/recv. `Command::Desktop` opens a remote `DesktopV2` session
on the pinned connection in **relay mode** (`SessionOpts::relay_encoded`); the
reply is `Reply::DesktopOpened` with the negotiated session and caps, then the
socket switches to a bidirectional body channel:

- Down, `DesktopDown::Frame { header }` + a big-endian u32 length + raw
  encoded payload (≤ 32 MiB); `DesktopDown::Event(DesktopEvent)` for control
  events; `DesktopDown::Finished` as the explicit clean end marker.
- Up, `DesktopUp::Control(DesktopControl)` verbatim into the remote session's
  control sender; `DesktopUp::Finished` or caller EOF ends the channel.

Encoded payloads travel beside postcard framing deliberately: a
`DesktopDown::Frame` postcard message stays under the 64 KiB control bound
while frame bodies reach 32 MiB. The manager never decodes or links a codec —
it republishes the remote session's encoded tap (sequence checks and
headers still run there), so a headless manager build can serve desktop.
The viewer owns decode: `rds_desktop::client::RelayDecoder` reapplies
wait-for-keyframe/broken-chain discipline on `RelayedFrame` payloads and
reports `NeedIdr`, rate-limited like the in-session path; the caller
forwards it with `request_idr` over its own control channel. Input
sequence numbers and heartbeat timestamps are stamped viewer-side
(`ManagedControl`, cloneable, serialized behind one writer half so
concurrent senders cannot interleave postcard bytes).

The channel ends on viewer `Finished`/EOF, remote-session end (both taps
close) or a body error; the manager then drops the remote
`DesktopSession`, aborting its tasks and releasing the stream permit.
`Client::desktop` shares the connect/response deadline; the body itself
has no IPC timeout. `--direct` still opens a `DesktopSession` in-process
for native/direct desktop use, unchanged.

## Remaining sequence and exit checks

1. **W2.4 migration:** the viewer manager API is implemented (managed
   desktop channel, local wire v5); coordinated installed-binary migration
   remains. CLI connectivity defaults and cooperative same-key-inode
   runtime ownership are implemented. Qualify native macOS credentials,
   actual distinct-user rejection, relay-registration reuse and
   FD/RSS budgets. Single-file send/receive are implemented;
   directory/two-way synchronization remains W8.
2. **W5 SSH:** native client, explicit host pins, credential selection, OS PTY
   requests and terminal restoration are implemented. Complete GDS host/account
   provisioning, broker isolation and authorized reattachment; qualify native
   macOS, two terminals while switching and failure without duplicate exec.
3. **W6 viewer:** newest-frame rendering and focus ownership; release pressed
   keys/buttons before changing devices. Reuse manager service APIs. Qualify
   native Linux backends and macOS capture/input permissions on real devices.
4. **GDS and operations:** enrollment/inventory, issuer/renewal/reconciliation,
   managed service leases, diagnostic bundles and private alert delivery.
5. **Release acceptance:** LAN/WAN, NAT, blocked UDP, relay loss, suspend/resume,
   revocation under load, simultaneous SSH/desktop/sync, long runs and measured
   setup/p99/input latency. Keep iroh default until owned parity is demonstrated.

Each step needs code, failure tests, platform evidence and an updated receipt.
All remediation waves remain open. This sequence preserves every other task in
the [full remediation plan](remediation-plan.md).


## Separated desktop events (additive local v5 extension)

`DesktopSeparated` replies with `DesktopSeparatedOpened` and an opaque random
route; `DesktopEvents` claims that route exactly once and returns
`DesktopEventsOpened`. Both requests pass the existing same-UID socket identity
and directory checks. All existing command/reply discriminants stay unchanged;
older managers reject the new commands. No remote ALPN, grant or frame format
changes. The native viewer and local agent need a coordinated update; the
legacy combined/headless APIs remain available explicitly.

The first socket carries ordered encoded video and upward controls. The second
carries only `DesktopDown::Event` messages and clean EOF; a frame there is a
protocol error. The attachment-ready fence follows the successful subscription
reply. An unattached owner waits at most five seconds. The bounded route registry
holds no peer connection and its guard removes/cancels a route on every exit.
Routes cannot be claimed twice or reused after cancellation/manager restart.

Each desktop/event socket uses a stream permit (64 total), and the existing
96-worker limit also applies. Queues hold 128 small events, one queued encoded
message and one being read. Encoded FIFO and 32 MiB payload bounds are unchanged.
The event writer has a two-second per-message limit; queue overflow fails the
desktop explicitly, rather than dropping an ACK and claiming healthy progress.
Both owned client readers abort on desktop drop. Subscriber EOF, unexpected
caller bytes, video/control EOF or remote end cancels only this desktop. The
manager's unrelated TCP/sync sessions stay live.
