# Record anchor recovery after exhausted storage

An anchor capacity error after the database committed previously left FileStore
unavailable until reopen even after disk capacity returned. Keep the exact
attempted metadata successor in memory for this narrowly classified failure.
On the next access, validate both protected anchor history and database metadata,
then durably finish the pending anchor under the existing owner locks. Never
retry an uncertain database I/O operation or adopt unrelated state.

Native fault tests cover every atomic-anchor checkpoint, repeated full-storage
failure, ownership exclusion, exact signed publication, durable deletion and stale
revision refusal after recovery/reopen. Replaced anchor or database metadata
permanently blocks in-process recovery. Existing injected generic write failures
retain their reopen-only contract.

Validation: rds-discovery library tests (68 passed, one explicit ignored child
fixture), strict Clippy for all rds-discovery targets, formatting and diff checks.
No wire schema, endpoint identity or desktop implementation is changed.
