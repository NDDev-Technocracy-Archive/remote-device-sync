# Retained offline release-tool prototype

This is unfinished work retained for
[the release-tool migration](https://github.com/NDDev-Technocracy-OpenNetwork/nddev-remote/issues/247).
The active workflows still use the existing scripts. This source has only
passed build-free formatting, dependency and whitespace checks and locked
metadata resolution. It has not been compiled or tested.

The prototype streams native runtime archives, verifies the unchanged design
package, assembles native Mac bundles and checks exact source, version, tag,
producer, security evidence and artifact identities before promotion. It has
no network, device installation or enrollment command. Synthetic tests include
wrong tags and versions, dirty source, late-page alerts, incomplete scans,
unsafe or duplicate archive members and tampered native package receipts.

Before switching any caller, compile with denied Clippy, run those fixtures on
Linux and macOS runners, review bounds and cancellation/child-process behavior,
and qualify real native bundles. Keep immutable source/tag and code-signing
requirements. Remove the superseded scripts only after the full caller
transition passes. This prototype is not part of an installed release.
