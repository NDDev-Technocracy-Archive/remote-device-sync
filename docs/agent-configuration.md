# Agent configuration

`rds-agent` accepts `--agent-config FILE`: a versioned JSON document covering
the agent's role, data-plane services, peer allowlist, authority posture,
admission limits and timeout policy. It is the policy counterpart of
[`--endpoint-config`](endpoint-configuration.md), which stays limited to the
endpoint transport. The file carries no secrets — issuers and
registry/revocation entries are verifying keys, and `state`/`ca` fields are
only filesystem locations.

## Precedence and validation

Precedence is built-in defaults, then the file, then explicitly supplied
flags — the same convention as the endpoint configuration. Repeated list
flags (`--allow`, `--issuer`, rotation receipts, `--service`,
`--no-service`) replace the file's whole corresponding list; an absent flag
preserves the file value. A flag selecting a role or service set replaces
whichever the file configured.

`schema_version` is required and equals 1. Files are limited to 16 KiB,
must be regular files, and unknown fields, duplicate fields, unknown
variants and unsupported versions are rejected. Structural validation —
mutual exclusion, cross-field requirements, numeric bounds — runs before
identity creation or socket binding, alongside the endpoint preflight.

## Sections

| Field | Meaning / bound |
|---|---|
| `schema_version` | Required integer 1 |
| `role` | `access` \| `sync` \| `desktop` \| `full`; mutually exclusive with `services` |
| `services` | Explicit data-plane list `["tcp","desktop","sync"]` |
| `disabled_services` | Services subtracted from the resolved set |
| `service` | `ssh_target`, `tcp_targets`, `allow_any_tcp`, `sync_dir` |
| `peers` | `allow`: endpoint-id strings, at most 256 |
| `authority` | `issuers`, `grant_ttl_secs` (1–86400), `tenant`, `policy_min_revision`, `directory`, `directory_ca`, `record_ttl_secs`, `record_state`, `registry`, `revocations` |
| `limits` | `max_connections`, `max_streams` (positive 16-bit); `max_fds`, `max_rss_mb` (positive, optional process ceilings) |
| `timeouts` | `handshake_secs`, `hello_secs`, `authz_secs`, `shutdown_secs`; each 1–3600 |

`registry` holds `key` (required when present), `epoch`, `state` and
`rotations`. `revocations` holds `key` (required when present), `epoch`,
`state`, `interval_secs` and `rotations`. Registry, revocation and record
fields all require `authority.directory`; revocations additionally require
at least one issuer — the same requirements the flags carried, now
enforced on the merged document so file and flag sources mix freely.

`authority.tenant` pins the grant v3 tenant claim: every grant must carry
the same tenant, and unscoped grants (including all version-2 grants) are
refused at authorization. `authority.policy_min_revision` sets a floor on
the grant's claimed estate policy revision, retiring older-policy grants
without waiting for expiry. Either binding requires at least one issuer —
grants are not evaluated without them, so the pin would silently do
nothing. Flag equivalents: `--tenant`, `--policy-min-revision`.

## Service enablement

`Ping` and `Info` are the always-on control plane and are never gated.
The gateable data-plane services are `tcp`, `desktop` and `sync`; `audio`
is wire-reserved but unimplemented and is rejected rather than silently
accepted.

- **No `role`/`services`/`disabled_services`** — the implicit set: `tcp`,
  plus `desktop` when the build has the `desktop` feature, plus `sync`
  when a `sync_dir` is configured.
- **`role`** — `access` = `{tcp}`, `sync` = `{sync}`, `desktop` =
  `{desktop}`, `full` = `{tcp, desktop, sync}`.
- **`services`** — exactly the listed services.
- **`disabled_services`** — subtracted from whatever the role, list or
  implicit set resolved to.

An enabled service whose prerequisites are missing fails validation:
`desktop` requires the `desktop` build feature, `sync` requires a
configured `sync_dir`. A stream greeting for a disabled service is refused
with `service <KIND> not enabled on this agent` before any grant or
per-service machinery runs — the deployment gate answers first, ahead of
grant scope checks. `Info` advertises exactly the served set, and the
directory record publishes the same list (Ping plus enabled data-plane
services).

`service.tcp_targets` permits extra `TcpConnect` destinations beyond the
single SSH socket; the flag surface has no equivalent list flag.

## Timeout policy

`timeouts.handshake_secs` bounds the inbound connection handshake;
`timeouts.hello_secs` bounds the `StreamHello` read on every new stream.
`timeouts.authz_secs` bounds the authorization-path replies (refusal and
final `HelloAck` writes); `timeouts.shutdown_secs` bounds the join wait
for established connection tasks when the agent stops. Defaults: 15s,
15s, 15s, 5s; each accepts 1–3600. Flags `--handshake-timeout`,
`--hello-timeout`, `--authz-timeout`, `--shutdown-timeout` override file
values. Per-service budgets (frame streams, transfer deadlines, renewal
windows) remain service-internal and are not set from this file; client
dial, idle and media/progress classes are still W2.6 open items.

## Process resource ceilings

`limits.max_fds` and `limits.max_rss_mb` bound the whole process, not a
single connection: while the kernel reports the agent holding that many
open descriptors (Linux `/proc/self/fd`, macOS `/dev/fd`) or that much
resident memory (Linux `VmRSS`, macOS `proc_pidinfo`), new connections are
refused at admission — the pending handshake is dropped before a slot or
task is consumed — until usage falls below the ceiling again. Refusals
emit `ConnectionBudgetExhausted` like the connection semaphore; the
observed `rds_agent_process_fds` and `rds_agent_process_rss_bytes` keys
appear in the metrics snapshot wherever the kernel reports them.
Platforms without an observable quantity keep serving rather than
gating on a guess. Flags `--max-fds` and `--max-rss-mb` override file
values. Per-service fairness and disk/media job budgets are still open
under W2.5.

## Example

[Access role](../examples/agent-access.json) keeps the historical default:
TCP forwarding to the configured SSH socket, allowlisted peers, default
timeouts spelled out explicitly. A sync-only deployment looks like:

```json
{
  "schema_version": 1,
  "role": "sync",
  "service": { "sync_dir": "/srv/sync" },
  "peers": { "allow": ["<endpoint-id>"] },
  "timeouts": { "handshake_secs": 10 }
}
```

which is equivalent to `rds-agent --role sync --sync-dir /srv/sync --allow
<endpoint-id> --handshake-timeout 10`.
