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
| `authority` | `issuers`, `grant_ttl_secs` (1–86400), `directory`, `directory_ca`, `record_ttl_secs`, `record_state`, `registry`, `revocations` |
| `limits` | `max_connections`, `max_streams`; positive 16-bit |
| `timeouts` | `handshake_secs`, `hello_secs`; each 1–3600 |

`registry` holds `key` (required when present), `epoch`, `state` and
`rotations`. `revocations` holds `key` (required when present), `epoch`,
`state`, `interval_secs` and `rotations`. Registry, revocation and record
fields all require `authority.directory`; revocations additionally require
at least one issuer — the same requirements the flags carried, now
enforced on the merged document so file and flag sources mix freely.

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
Both default to 15 seconds and accept 1–3600. Flags `--handshake-timeout`
and `--hello-timeout` override file values. Per-service budgets (frame
streams, transfer deadlines, renewal windows) remain service-internal and
are not set from this file; global timeout classes are W2.6 work.

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
