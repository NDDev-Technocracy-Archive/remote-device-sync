# nddev-remote — agent instructions

Public standalone remote-screen product of NDDev OpenNetwork, retaining the
RDS engine and its history. Start with `docs/product.md` and `docs/integration.md`.
The native client is `nddev-remote`; the existing `rds-*` crates own the media,
transport, session and authorization implementation. NDS integrates through an
adapter and must not become a required account, telemetry or synchronization
dependency of this product. Preserve existing endpoint identities, wire formats,
consent, stored workspaces and explicit scope while migrating callers.

## What this module is

A Cargo workspace under `crates/` — see `docs/architecture.md` for the
crate map and dependency direction. Estate, tenant, host and credential
facts do not belong here — the module is public and must stay free of
private data.

## Before changing anything

- `docs/architecture.md` — crate map, protocol decisions, identity model
- `docs/research.md` — deep research incl. the full-ownership plan (§9)
- `docs/platforms.md` — supported OSes (Linux x86_64, macOS arm64),
  backend probe orders, cfg conventions
- `docs/conventions.md` — layering, safety, errors, wire, testing rules
- `docs/roadmap.md` — milestone gates; a change belongs to a milestone
- `docs/implementation-plan.md` — workstreams, tasks, tests, exit criteria

## Checks

```sh
just check  # build-free formatting, workflow, dependency and whitespace checks
just test   # denied Clippy, nextest and doctests; build devices/runners only
just test rds-desktop/x11  # Linux native feature lane
scripts/checkpoint.sh <gate>   # wave-close gate; registered ids only
```

CI runs fmt + clippy + test on `ubuntu-latest` and `macos-latest`.
`rds-bench` is the measurement harness; reports land in
`docs/reports/` and are part of every wave's checkpoint evidence.

## Git

Conventional Commits, signed commits, `main` is pull-request only.
