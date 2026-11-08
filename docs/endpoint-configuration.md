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
| `keep_relays_connected` | Optional bool, default false; keep 1–3 custom Iroh relays independently registered and advertise connected registrations |
| `prefer_relay_order` | Optional bool, default false; prioritize relay URL order among eligible relay paths while retaining faster direct paths; requires persistent registrations, latency preference and multiple paths |
| `max_multipath_paths` | Optional integer 1–32; absent preserves the backend default |
| `transports` | `all` (default), `direct-only`, or `relay-only`; bounds usable path kinds |
| `packetization` | `adaptive` (default), or `conservative`: 1200-byte primary QUIC UDP payloads, MTU discovery and GSO disabled |
| `path_preference` | `backend-default` (legacy policy), or `latency`: lower measured RTT across allowed direct/relay paths; explicit single-path pins still win |
| `congestion_control` | `bbr3` (default), or `cubic`; explicit local controller selection for measured qualification |

Congestion selection is independent of stream priority, path kinds, packetization,
receive/send windows and authorization. Both Iroh and owned Noq instantiate the
selected pinned engine controller. The default BBRv3 field is omitted on
serialization, retaining legacy file shape. Older strict binaries refuse explicit
new fields; update binaries before selecting a controller. No controller is
declared universally fastest or most stable by adding this option: qualify
latency, delivery and recovery on the actual path before adopting it.

The conservative policy supports qualification of tunnel/packet-inspection paths;
it is not a claim that every such path needs it. It leaves congestion control,
receive/send windows, keepalive, TLS and endpoint identity unchanged. Relay
encapsulation has its own outer transport; the limit describes the primary RDS
QUIC datagram, not every packet of an independently attached relay connection.
Path kinds and packetization apply at bind, without modifying OS routes or VPN
settings. Direct-only refuses configured relays; relay-only on iroh requires an
explicit relay. Library validation supplies the same checks as the file API.

Both new fields are omitted when serializing their defaults, preserving the old
default file shape. Strict older binaries reject explicit new fields; install a
compatible binary before applying them. Real loopback proxy tests echo 256 KiB
in both directions on Iroh/Noq, require all payload bytes to cross the proxy and
inspect actual UDP lengths for the 1200-byte bound. These tests do not establish
WAN latency, native input responsiveness or a preferred production policy.

At library bind entry points, `EndpointConfig` also validates 1–16 distinct
ALPN identifiers of 1–255 bytes. `EndpointSettings::into_endpoint` performs
validation without creating an identity or socket. Low-level backend entry
points validate the backend selected by their function name; injected transports
must supply an attached relay handle consistent with their relay configuration.

## Reachability presets

An explicit `keep_relays_connected: true` keeps the initially configured custom
Iroh relay connections alive across peer inactivity. Registration completion,
failure and task exit update advertised addresses; a configured URL alone is
not readiness. Removing an origin withdraws it; dynamically adding another
origin does not expand the initial persistent-registration budget. This option
requires 1–3 custom origins and is refused for the owned backend or a public
preset. Its default is omitted on serialization. Older strict binaries reject
the explicit field and must be upgraded before deployment.

Registration is distinct from a validated end-to-end QUIC path. The existing
latency preference can maintain previously known standby paths and select a
confirmed alternative; a direct path remains eligible. Configuring persistent
registrations does not override path pins, controller choice, authorization,
OS routes or directory authority. Private deployment assigns host roles. The
optional `prefer_relay_order` uses the existing ordered URL list as its relay
priority source. Healthy configured relay tiers outrank later relay tiers
regardless of their RTT; direct paths still compete by RTT against that eligible
tier. Failed or unconfirmed paths cannot outrank a positively confirmed
alternative. Defaults and explicit pins retain their prior selection semantics.

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

## Measured latency preference

`path_preference: latency` removes Iroh's unconditional direct-primary/relay-
backup ranking and requests a1s bounded reselection interval. A5ms minimum gain
keeps the current path under small fluctuations. The owned Noq backend already
periodically ranks validated live paths by RTT with the same stickiness.
Transport bounds and max_multipath_paths1 remain authoritative; no OS/VPN route,
identity, authority, congestion-controller or wire setting changes. Defaults
retain backend policy, and old strict binaries reject the new explicit field.

Published Iroh 1.4.1 invokes its selector on topology changes only. The exact
published source is temporarily retained under vendor/iroh with an opt-in
refresh hook: default selectors remain topology-only; intervals are bounded to
250ms..60s, and periodic refresh does not reapply an unchanged selection. See
the patch/provenance notice. No transport engine or cryptographic behavior is
changed. A low standby RTT does not prove bulk capacity; qualify native latency,
quality and migration under actual mixed load before adoption.
