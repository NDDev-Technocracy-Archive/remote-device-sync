# Native clipboard body ownership

Outgoing body admission now precedes native representation reads. The response
guard is the single-use sender for that request, and its original owner travels
into the native read. Incoming verified bytes keep their body reservation until
native publication drains. Canceling an async waiter cannot return either slot
while the admitted native operation still executes.

The native provider contract requires that per-operation original owner for
reads and publication. On macOS it survives queued and running AppKit jobs;
the factory/selection authority remains independently retained by selected files.
Selected-file streaming uses the same response admission and keeps its existing
length/integrity checks. The DesktopV5 wire is unchanged. These building blocks
do not advertise negotiated rich clipboard.

Source commit: `25e1b8f935243b9764837ea12cfb0bade2a53f59`.
Source snapshot SHA-256:
`e60c7ad779139493fe3607d15eef91ab0fc157a1be60fc81d032a26de26c87bf`.

Physical Ubuntu x64 and macOS arm64 passed all-target denied Clippy,
desktop/CLI/agent library and executable tests, and 19 packaging tests each.
Desktop results were 228 passed with six existing display cases ignored on
Linux and 196 passed on macOS. Two additional loopback QUIC cases block actual
filesystem work: canceled outgoing representation reads and canceled incoming
native publication retain their directional body slot until the real operation
returns. Existing selected-file cancellation/integrity cases remain unchanged.

The optimized private-pasteboard probe built from this snapshot has SHA-256
`4801d40dfb60fd7cebf8236f1cef47a93a8b2353b28a66bdfdd8bd2b9a652a05`.
Strict native signature verification and all 14 named-pasteboard cases passed
on physical M1 and M2 using those same bytes. The native framework's unsuccessful sandbox-extension consume
for an unscoped test URL remains recorded; ordinary file access succeeded. This
does not qualify sandbox access, general-clipboard behavior or paste into a
receiving application. No installed Host, OS grant or endpoint identity changed.
