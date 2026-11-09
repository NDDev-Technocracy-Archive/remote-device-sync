# Agent stop signals before readiness

A broad native workspace test observed the agent's authenticated admin endpoint
working, then an owned SIGTERM ended the process unsuccessfully. The stop future
was created before readiness, but its async function had not executed yet: Unix
handler registration happened only when the final serving select polled it.

The agent now constructs both Unix SIGTERM and SIGINT streams synchronously
before endpoint startup and admin readiness. Its returned future only waits on
the already registered streams. Registration errors propagate to startup rather
than panicking. No listener, timer, dependency or persistent setting was added.

Frozen source
`3fa1d65fa21b6468e6362a35d7e6d093734cef6f0f5a2e6a95ef53beaae2b6fd`
passed denied agent Clippy with portal/Noq features on physical Linux. Its
subprocess regression constructs the waiter, reports a pipe barrier and receives
an actual SIGTERM or SIGINT before that future is ever polled. After releasing
the child, both waits complete successfully. The test owns both processes and
their pipes through completion; no installed process or user input is involved.
The authenticated admin endpoint and signal-cleanup integration case also passed.

Counterexample source
`26bc410e29282fe0d345fa9b894cb71f01e735c5c0e5c572c907140271a0e409`
changes only the function body to defer registration into the returned async
block, retaining the caller signature and exact regression fixture. The child
terminates with signal 15 (SIGTERM); the parent test fails normally with exit 101.
Keeping child stdout open through its harness completion excludes a pipe-close
failure from this accepted comparison. Neither source uses a startup sleep,
retry or weaker signal assertion.

This report establishes the Unix process-lifecycle correction. Cross-platform
CI remains required before merge; it is separate from installed host upgrades,
native screen/input permission adoption and cross-device clipboard acceptance.
