# Superseded journal collection — 2026-09-28

Scope: W1.10 inactive/legacy journal collection. Linux, synthetic
fixtures. No physical power-loss or native macOS qualification is
claimed.

## What existed

Every dead transfer left its `.rds-sync/<root-hex>/{meta,parts/}` state
permanently: `Journal::open` creates one content directory per offered
root and only the *completed* journal's own `cleanup` removed anything.
Repeated sends of changing content to the same destination accumulated
verified parts forever — unbounded private-state growth under a dir the
user cannot see.

## What "collectible" means here

A resume always opens the *offered* root: `Journal::open` keys its state
dir on `hex(manifest.root)`. Once a different root is offered for the
same destination, every older journal for that `rel_path` is
unreachable resume state — provable inactivity, no timeout or heuristics
involved. Journals for *other* destinations stay: they remain resumable.

Attribution is cryptographic, not filename-shape: a candidate dir is
collected only when its `meta` exists, carries a verified BLAKE3
trailer, decodes to the same `rel_path`, **and** `hex(meta.root)` equals
the directory name. A hex-named dir with a torn, foreign or
root-mismatched meta cannot be attributed and is preserved, as are all
non-hex names.

## Implementation

- `Directory::children()` — name-only enumeration of a pinned dir
  (getdents on the held fd; nothing resolved or followed; `.`/`..`
  never reported).
- `decode_meta`/`read_meta` — bounded (`8 KiB`) postcard+trailer decode
  of `meta`.
- `collect_superseded(state, rel, own)` — runs inside `Journal::open`
  under the held receive locks. Any journal dir seen while the lock is
  held belongs to a dead transfer by definition (the lock serializes
  all opens for the root). Best effort: per-entry failures are warned
  and skipped — a torn sibling must never block a fresh transfer.
- `collect_journal` removes only names this engine could have written:
  hex-named parts and the reserved `pending`/`meta` are re-proven
  regular single-link files via `discard_owned` before unlinking —
  a planted symlink or foreign file aborts collection of that journal
  instead of deleting foreign data. `rmdir` refuses non-empty, so any
  surviving residue keeps the directory, matching `cleanup`'s
  never-sweep-the-unknown contract.
- Legacy destination-dir staging names (`*.rds-part` and friends) stay
  preserved: ownership of a user-visible name cannot be proven, which is
  exactly why `discard_owned` never swept them.

All work runs on the bounded `disk_job` pool (`Journal::open` already
executes there); enumeration is finite — one listing per `open`.

## Verification

`superseded_journals_are_collected_and_foreign_entries_survive` plants
dead journals directly (bypassing `open`, which would collect at plant
time) and asserts: clean superseded journal fully removed; proven parts
removed while a foreign file keeps its journal dir; different-`rel`
journal untouched and still resumable; malformed 64-hex dir,
non-hex sibling and a valid-meta-wrong-name dir all preserved.

Checks: `cargo test -p rds-sync --lib` green (21 tests incl. the new
one); fmt, clippy and workspace lanes run before push.

## Still open under W1.10

Physical power-loss qualification, native macOS, large-file campaign.
