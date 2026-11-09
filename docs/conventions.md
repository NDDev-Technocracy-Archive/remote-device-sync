# Engineering conventions

Rules every change follows. CI enforces what it can; the rest is review.

## Dependency and layering rules

- Dependencies point strictly down the crate map in
  `docs/architecture.md`. Adding an upward or sideways dep is an
  architecture change: discuss it in the PR body.
- `rds-core` is a leaf: no io, no async runtime, no platform code.
  `crates/rds-core/tests/layering.rs` enforces the manifest side.
- Shared identity/address types (`EndpointId`, `EndpointAddr`,
  `RelayUrl`, `SecretKey`, `TransportAddr`) are owned by `rds-core`
  and re-exported by `rds-net`. Backend-native types (iroh, noq)
  never appear in service surfaces — convert inside the backend
  adapter (`rds_net::backends::iroh::convert`).
- No new third-party dependency without need: prefer standards,
  thin FFI bindings to OS/driver APIs, and our own protocol code.
  Justify in the PR description; `deny.toml` gates licenses.
- Public API of a crate lives in `lib.rs` re-exports; platform modules
  may be `pub` but unstable surfaces are `#[doc(hidden)]` or doc-noted.

## Safety

- `unsafe` is confined to platform-backend modules (`rds-desktop`
  capture/codec/input and native AppKit icon/quality/clipboard adapter, `rds-net` socket layer, and the read-only
  `rds-discovery/clock/macos.rs` boot-identifier adapter). Workspace lint flags it
  everywhere else; backends opt out per module with a `// SAFETY:` note
  per block.
- Never trust the wire: every decoder/parser bounds its inputs; every
  record verifies before use (`rds-discovery` does this on `put`).

## Errors and async

- Library crates return typed errors (`thiserror`); binaries may use
  `anyhow`. No `unwrap`/`expect` outside tests and
  impossible-invariant paths (commented).
- Tokio is the only runtime. No `std::thread` for core loops;
  `spawn_blocking` for sync FFI (capture backends, codecs).
  `rds-observe` isolates synchronous stderr in one bounded output adapter
  thread, outside service/transport loops; a stuck OS write must not hold
  Tokio runtime shutdown. See [observability](observability.md) for the
  bounded wait and explicit possible record loss.
- No unbounded queues on latency paths: bounded `mpsc`, drop-stale
  policy at the producer, never let backlog accumulate.

## Wire protocol

- `rds-core` owns RDS control wire types; postcard + explicit length prefix;
  64 KiB max frame on control paths. The async frame read/write half lives
  in `rds-net::wire` (re-exported as `rds_net::{read_frame, write_frame}`)
  so `rds-core` stays runtime-free. Media streams carry raw codec
  bitstream with a fixed header — no serde in the hot path.
- Standard SSH framing, key exchange and key formats belong to `russh`, behind
  `rds-ssh`; do not duplicate them as RDS control messages.
- Transport protocol selection uses ALPN (`rds/0`, `rds-relay/0`). Signed
  objects and local IPC also carry independent explicit versions: [grant v3
  and IPC v5](grant-leases.md) require a coordinated upgrade without weakening
  authorization — v2 grants still verify, but a deployment that pins the v3
  tenant/policy claims refuses them. General remote capability negotiation
  remains W2.2.

## Platform code

- One backend per file; `#[cfg(target_os)]` on the module declaration,
  not inside functions. Preference orders are fixed and documented in
  `docs/platforms.md`.
- A backend that probes unavailable returns `Ok(None)`; present-but-
  broken returns `Err` — and the demotion is logged.

## Testing lanes

- `just check` runs build-free formatting, workflow, dependency-graph and
  whitespace checks. Native compilation, tests and applications run on build
  devices or runners; the source-check entrypoint does not start them.
- `just test` runs denied Clippy, nextest and doctests on build devices/runners.
  Its optional feature argument selects the native feature set explicitly.
- `cargo fmt --check`, `cargo clippy --workspace --all-targets -- -D
  warnings`, `cargo test --workspace` — green on Ubuntu and macOS in CI.
- Feature lanes: `--features rds-desktop/x11` on Linux;
  `--features rds-agent/desktop,rds-cli/desktop` on both OSes — the
  desktop feature must compile everywhere, not only where it runs.
- Protocol/interop tests live in `tests/` of the owning crate; e2e
  transport tests must not require a display.
- Benchmarks are acceptance gates for the transport and media work —
  numbers land in `docs/reports/` per roadmap.

## Native build cache

Native CMake dependencies use an out-of-source build directory and a local
install prefix under the Cargo output. Reuse a configured native cache only
with the same compiler/toolchain identity. If that identity changes, preserve
the failed evidence and configure a fresh scoped build tree with all required
flags together. An unexpected system install path is a configuration failure;
do not elevate privileges or widen global directory permissions to finish it.
See [CMake build trees](https://cmake.org/cmake/help/latest/manual/cmake.1.html)
and [install prefix](https://cmake.org/cmake/help/latest/variable/CMAKE_INSTALL_PREFIX.html).

## Commits and docs

- English only. Signed commits on `main` (ruleset enforced).
- Docs-as-code: architecture decisions in `docs/architecture.md`,
  research in `docs/research.md`, platform facts in
  `docs/platforms.md`. A design change that doesn't update its doc is
  incomplete.
