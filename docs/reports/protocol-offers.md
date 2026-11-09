# Explicit protocol offer qualification

Execution: [clipboard integration #212](https://github.com/NDDev-Technocracy-OpenNetwork/nddev-remote/issues/212).
This increment owns the transport prerequisite for selecting an implemented
desktop protocol with an older peer. Manager/GUI migration, OS clipboard
publication and actual receiving-application pastes remain open.

`rds-net/tests/protocol_offer.rs` uses actual loopback QUIC connections, completed
TLS handshakes, peer identities and datagram payloads. With `transport-noq`, its
cases exercise all four Iroh/owned client/server combinations. The default
feature lane separately exercises Iroh. The cases cover:

- Server preference and repeated different offers on the same endpoints.
- A peer that supports only the explicitly offered legacy protocol.
- An exact legacy caller whose peer prefers a newer protocol.
- Refusal when the caller excludes the peer's available protocol.
- Concurrent exact and alternative offers retaining their own protocol/data.
- Malformed and locally unconfigured offers refusing before resolution.
- A valid protocol offer refusing an incorrectly pinned remote identity.

The original four owned exact-ALPN regressions remain in
`rds-net/tests/alpn_selection.rs`. Native Linux and macOS CI run those cases,
the new default/owned cases, and denied-warning all-target `rds-net` Clippy.
The cases establish neither successful TLS resumption nor relay, WAN, OS
clipboard or input-to-visible performance. The fixture protocol names are not
production capability advertisements. Source checks run without a build on
the coordination host; executable outcomes are recorded on the pull request
with the exact native producer jobs before merge.
