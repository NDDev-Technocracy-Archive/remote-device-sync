# Endpoint configuration

Both `rds` and `rds-agent` accept `--endpoint-config FILE`. This versioned JSON
file controls the endpoint transport. It does not contain the endpoint secret
or service policy; role, service, peer, authority, limit and timeout policy
live in the versioned agent configuration — see
[agent configuration](agent-configuration.md).

Connectivity commands now use the local agent by default. Put transport/directory
configuration on that agent; managed CLI commands reject those flags. Explicit
`rds --direct` commands retain this endpoint configuration surface and require
an unused persisted identity. `rds id` remains an offline configuration/key
reader. See [local manager migration](local-sessions.md).

## Precedence and validation

Precedence is built-in defaults, then an explicitly selected file, then explicitly
supplied flags. There is no implicit config search or environment override for
these transport fields. Existing key-path and logging environment behavior is
unchanged. Repeated `--relay` and `--bind-address` flags **replace** the file's
whole corresponding list. An absent flag preserves the file value. Backend
changes do not silently discard incompatible relay settings.

`schema_version` is required and currently equals 1. Files are limited to 16 KiB
and must resolve to regular files. Unknown fields, duplicate fields, unknown
variants, trailing JSON and unsupported schema versions are rejected. The
selected backend must be compiled into the binary; `noq` requires the
`transport-noq` feature. Syntax is parsed before overrides, and the resulting
combination is validated before creating a key file, opening policy state or
binding the endpoint. The same preflight window applies to the merged agent
configuration (role/service/authority/timeout sections).

| Field | Meaning / bound |
|---|---|
| `schema_version` | Required integer 1 |
| `backend` | `iroh` (default), or feature-enabled `noq` |
| `bind_addrs` | Socket addresses; at most one on iroh, at most 16 entries on noq; fixed addresses are distinct, repeated port 0 requests allocate separate ephemeral sockets |
| `relay` | One explicit reachability preset below |
| `max_multipath_paths` | Optional integer 1–32; absent preserves the backend default |

At library bind entry points, `EndpointConfig` also validates 1–16 distinct
ALPN identifiers of 1–255 bytes. `EndpointSettings::into_endpoint` performs
validation without creating an identity or socket. Low-level backend entry
points validate the backend selected by their function name; injected transports
must supply an attached relay handle consistent with their relay configuration.

## Reachability presets

| `relay.mode` | Behavior |
|---|---|
| `default` | Iroh public relay and public address lookup preset; owned transport is direct-only |
| `disabled` | Direct-only, with no backend public relay or public lookup |
| `iroh` | `urls`: 1–8 distinct HTTP(S) relay origins; public address lookup disabled |
| `owned` | `route`: one key-pinned owned relay locator; optional `limits` below; requires noq; public lookup disabled |

Iroh origins have no credentials, non-root path, query or fragment and are at
most 512 bytes each. Owned relay locators use
`rds-relay://PUBLIC_HEX_KEY@IP:PORT`: the public identity belongs to the **relay**,
not this device's endpoint. The shared discovery parser validates the locator.
The current owned bootstrap consumes exactly one usable direct IP address;
multi-candidate bootstrap/racing remains W3 rather than silently dropping extras.
Owned route parsing includes IPv6 syntax; this receipt does not establish all
IPv6 data-path combinations.

The flags `--relay URL` (repeatable), `--owned-relay ROUTE` and `--no-relay` are
mutually exclusive. `--backend` selects the transport and `--bind-address`
supplies UDP bindings. The GDS directory configured with `--server`/`--directory`
remains independent of backend public lookup and can still resolve names/records
when `--no-relay` is used.

The owned relay's outer connection binds an independent ephemeral port on the
primary interface. It must not rebind the already-owned primary UDP port. A
configured fixed primary port therefore remains usable with an owned relay.
Relay startup retry, address-family selection, cross-relay reachability and
TCP/443 fallback remain later work. The owned transport is not
promoted to the default by adding its configuration surface.

Owned mode accepts a strict optional `limits` object:

| Field | Default | Accepted values |
|---|---|---|
| `max_peers` | 1024 | 1–65535 |
| `datagram_queue` | 128 | 1–65535 |
| `peer_grace_secs` | 30 | 1–4294967295 |

For example, `"limits":{"max_peers":256,"datagram_queue":64}` changes two
budgets and preserves default grace. These are per attached client tunnel;
they do not configure the relay server or all underlying QUIC buffers.
See [peer ownership and receive bounds](relay-control.md#client-peer-ownership-and-receive-bounds).
Changing only `--owned-relay` preserves file limits. Selecting another relay mode
removes the inapplicable limits. Runtime custom limits without an owned relay
are rejected. Old version-1 files remain valid, and default limits are omitted
when serialized; old binaries with strict schemas reject explicit new fields.
This additive file setting changes neither the relay wire format nor the default
transport backend.

## Examples

- [Direct loopback](../examples/endpoint-direct.json) is usable for local tests.
- [Custom iroh relays](../examples/endpoint-iroh.json) uses reserved `.example`
  domains; replace them with independently provisioned relay origins.
- [Owned relay](../examples/endpoint-owned.json) uses a public
  [RFC 8032 §7.1 test-vector key](https://www.rfc-editor.org/info/rfc8032/) and
  documentation address, never a deployed relay or a private credential.

For example, `rds --endpoint-config examples/endpoint-direct.json id` loads the
configuration and prints the persistent local endpoint identity without binding
a socket. `rds-agent --endpoint-config FILE` uses the same transport schema;
its allowlist/grant and service policy must still be provisioned separately.

Roundtrip, feature isolation, malformed/oversized inputs, list replacement and
binary preflight are regression-tested. See the
[2026-09-25 receipt](reports/rds-endpoint-config-20260925.md) for exact scope and
remaining qualification.

## TCP service destinations

`rds ssh --remote`, `rds forward --remote` and `rds-agent --ssh` share
`rds_core::TcpTarget`. Accept `host:port` or `[IPv6]:port`, with ports 1–65535.
The library client validates the same host/port pair before opening a QUIC
stream; the agent validates wire input before policy and socket operations.
Malformed command-line targets fail before identity creation or endpoint bind.

IP literals normalize to canonical spelling, including IPv4-mapped IPv6.
Unspecified, multicast and IPv4 broadcast destinations are rejected. ASCII
hostnames are lowercased, limited to 253 bytes before an optional final DNS root
dot, and split into nonempty labels of at most 63 bytes. Local underscore aliases
are accepted. URL/userinfo syntax, ambiguous numeric IP shorthand and scoped
IPv6 are rejected; IDNs must use punycode. The final DNS dot is preserved because
it affects resolver search behavior.

Agent allowlist comparison and actual dialing use the same normalized target.
Equivalent IP spelling does not widen the permitted port or address; the
development `allow_any_tcp` mode still requires a valid target. This is syntax
and policy consistency, not DNS address pinning, SSH host-key verification or
an implementation of the SSH protocol. Those remain separate workstreams.
No wire tags or protocol version changed. See the
[TCP destination receipt](reports/rds-tcp-target-20260925.md).

## Agent admission options

`rds-agent --max-connections N --max-streams N` selects positive 16-bit
application budgets (defaults 32 and 64). Connections include pending handshakes;
streams are per connection. Invalid values fail during argument parsing before
key creation. These flags are independent of the transport JSON schema. See
[agent lifecycle](agent-lifecycle.md) for refusal, backpressure and cleanup.

## Client forwarding options

`rds forward` accepts `--max-connections N` (1–65535, default 64)
for their local listener's concurrent forwarding workers. This is independent
of the agent's connection budget. Invalid values fail before identity creation
or dial; these service options do not extend the endpoint JSON schema. See
[client lifecycle](client-lifecycle.md) for request deadlines and cancellation.

`rds ssh` opens an [embedded SSH session](ssh.md) on one authorized TCP stream;
its account/host-key/PTY options are independent of endpoint configuration.
