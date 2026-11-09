# Native clipboard cleanup acknowledgement

The queued macOS and X11 owners could release their running-operation guard
and native body credit before destroying the operation's original purpose.
Consequently, lease close could acknowledge drain while that cleanup still ran.

Both native job layouts now destroy the command and original operation context
before releasing body admission, the physical-core reference and finally the
running guard. This preserves the existing ownership contract on success,
queued cancellation and failure without another task, timer or runtime queue.

The regression performs an actual native text read and blocks destruction of
its original context after the native call replies. While that destruction is
blocked, close must remain pending and body credit must remain occupied. After
explicit release, close must complete with the credit returned. The fixture
uses a single poll to prove pending state; no delay or retry supplies success.
Its native workers are stopped before an expected baseline assertion is raised.

## Qualification

Linux final snapshot SHA-256:
`fe8e8c42823774a71fd32e9360b2226ced61258c7c7d374f6a66b900a0b5b223`.
On physical Ubuntu x64, denied all-target desktop Clippy passed, all 204
display-independent desktop library cases passed with 11 native cases outside
that invocation, and all eight X11 clipboard cases passed under a dedicated
X server. This includes the new real selection read/close regression and the
existing INCR, PNG, cancellation and simultaneous V5 media/clipboard checks.

Restoring only the two original native job layouts in snapshot
`27d20d92276fd4acadd564c709f3ac0cc233e1872edcac1707713e7f8a5fd7e6`
makes the same X11 regression fail: close acknowledges before original cleanup.
The failing test tears down its own native workers and exits normally with a
failed result. An earlier baseline invocation was stopped during preparation
while that fixture teardown was being corrected; it is not acceptance evidence.

Apple Silicon runtime snapshot SHA-256:
`bfc2f4c9295fb0d099fc8f32c0f93f3649d36fef0e67dcd95cba502201437ccb`.
It passed denied all-target desktop Clippy, all 201 desktop library cases and
the ordinary debug probe build. Its exact probe, SHA-256
`7ddfd6ad97edc39ca5839eb879c2751b532b535f29d760158924d4ecc2ad1240`
(11,671,776 bytes), passed strict native signature verification and all 18
named-pasteboard cases on physical M1 and M2 hosts. Later Linux fixture cleanup
changes do not change that macOS runtime or probe.

The Apple Silicon counterexample restores the original native layouts in
snapshot
`de41a26c5f90f5c049360018338972b91434400f3ff50ab3364738137d8220d7`.
Its ordinary debug probe built and passed strict signature verification, then
failed the new case with the specific early-drain/body-credit error. Its SHA-256
is `ddcf76247ed925fb074d63838445f5f9b738756e262179065ac0a295ba9fc5a2`.

The native permission warning remains recorded on both hosts. These isolated
pasteboard results do not establish sandbox or receiving-application acceptance.
The general clipboard, installed host signatures and OS permissions are not
changed by the probes. Negotiated native metadata and manager/viewer integration
remain separate work.
