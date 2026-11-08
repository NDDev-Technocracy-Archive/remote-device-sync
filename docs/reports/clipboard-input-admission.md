# Clipboard input admission

Legacy text paste previously enqueued chunks individually. When the bounded
input queue filled partway through a value, the receiver could see an incomplete
transfer and the viewer ended input for the screen. A clipboard request hitting
the same queue also ended the workspace's connection.

The sole UI producer now admits the complete clipboard message under the queue
lock before allocating its chunks. Admission checks the actual number of input
events in the immediately following paste chord. The consumer can only free
space between admission and that synchronous chord. A refused message changes
neither the queued input nor its sequence state and reports a clipboard notice
beside the connected screen. Keyboard/pointer queue failures retain their
existing handling. The DesktopV5 wire and text limit are unchanged.

Source snapshot SHA-256:
`171056a9987440b80e585739a69474358c5f5d4296684b180bd5362b9fcdf83e`.
Physical Ubuntu x64 and macOS arm64 checks passed with the normal development
toolchain: all-target denied Clippy, desktop/CLI/agent library and executable
tests, and 19 packaging tests on each platform. Desktop results were 229 passed
with six existing display cases ignored on Linux and 197 passed on macOS.
This is queue/lifecycle qualification, not cross-device paste acceptance.

Three regression cases exercise behavior at the real queue boundary:

- A two-message clipboard value refused against a nearly full queue allocates
  no chunks, preserves every existing input sequence and still accepts later
  input without an input-drop count or close message.
- Large UTF-8 and empty values either fit completely with their following chord
  or leave the queue unchanged. Admitted chunks assemble to the exact original
  text and retain chord ordering.
- A queued close refuses clipboard admission rather than appending work after
  the ended input attempt.

Native asynchronous providers, Linux viewer clipboard access, rich clipboard
negotiation and native image/file paste remain separate work in #212.
