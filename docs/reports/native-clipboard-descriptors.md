# Native clipboard descriptor admission

Every native selection now receives an explicit shared descriptor allowance.
Each held FD owns its reservation and keeps the physical budget alive through
reader/native-job completion. Closing a provider or replacing a native owner
cannot create another allowance while old files still occupy the first one.
FDs close before their reservations and original resource authorities, including
failed admission. There is no content pre-read or staging cache.

The common versioned native profile declares 64 retained selection descriptors,
independently of the protocol's 128 files per manifest and two directional body
transfers. Native body-job concurrency is also independent of wire concurrency.
All settings and nested limits are mandatory; no environment lookup, partial
JSON default or second editable native profile supplies a missing value.
`serde_json` was already in the lock and the desktop dependency graph; its
declaration moved from optional Linux portal scope to the common config owner.

Source snapshot SHA-256:
`27c6f3c019b67bdbb195a4d54821846cd5d6c229a0d8e38956802f3f53061ece`.
Physical Linux/macOS selected CLI/agent/desktop/core/sync all-target denied
Clippy passed. Default-feature desktop Clippy also passed. Core clipboard tests
(5 each), native selection tests (6 each), and clipboard tests (Linux 38 with
3 existing display cases ignored; macOS 34) passed. The last small source change
adds size-hint rejection before any path open; final all-target Clippy and all
six selection tests passed again on both platforms.

Tests use actual held files, rename/path replacement, capacity exhaustion,
cancellation during blocked native work and budget-pool replacement. A request
for a nonexistent path is refused as capacity exhaustion before native open
when the allowance is occupied. A canceled reader retains both its selected FD
allowance and body reservation until the actual blocking closure returns.

On physical M1 and M2 the same native probe passed 14 cases against private
named pasteboards, including the real NSURL-selected file and descriptor
exhaustion while its reader remains alive. The final optimized executable
SHA-256 is `0599a7301866c2f1cc4c29cc5ec52be2d3e627c2c508864747d660839a7f4d6c`;
strict native signature verification passed on each device. No general clipboard
contents or OS grants were read/replaced by the probe. The native framework
reported an unsuccessful sandbox-extension consume for the unscoped local test
URL; ordinary process file access succeeded. This does not qualify sandboxed
file access. The development SDK's missing LLVM strip library left
these optimized probe binaries unstripped; no SDK override was used.

This increment completes admission/configuration for native selected descriptors.
It does not advertise negotiated rich clipboard, publish native file promises,
or establish cross-device copy/paste. Native wire/manager/UI and platform
publication remain open in #212.
