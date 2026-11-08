# Native design system

The standalone product consumes the canonical Apache OpenNetwork design system,
not the NDS integration fork. The unchanged Rust directory comes from
[v0.0.1-alpha.9](https://github.com/NDDev-Technocracy-OpenNetwork/nddev-opennetwork-design-system/releases/tag/v0.0.1-alpha.9),
commit `dbdf11b27fc6245b7942ad3e271f171602fce931`, directory `packages/rust`.
The package has no runtime dependencies, I/O or allocation. The native renderer
binds its semantic colors, desktop metrics, typography, motion and shadows to
egui; it neither edits generated tokens nor imports NDS application services.

`packaging/design-system.json` records that release, its Git tree and all file
SHA-256 values. The directory SHA-256 is
`20aed9da97d34f34e5b49aff5a54bbcb546061867dbc0b703aad808c407a7b21`.
It hashes sorted UTF-8 relative names, each followed by NUL and its raw 32-byte
file SHA-256. `python3 scripts/design_system.py` verifies the exact inventory and
bytes, including fonts and licenses, and refuses extra files or symlinks.
Native packaging and artifact promotion run the same verifier and bind its pin
into build metadata. Updating the package replaces the entire released directory
and its reviewed pin; generated files are never edited in place.

**Appearance** lists the released theme identities: light, dark, light contrast
and dark contrast. An explicit selection controls subsequent menus and popups as
well as workspace panels. Tabbed and separate-display windows share that
workspace's appearance. A theme change repaints local chrome without restarting
a remote session or reloading fonts. **Save workspace** persists it through the
existing explicit, private atomic preference writer. There is no per-frame or
periodic theme write, runtime font fetch or second design token cache.

The renderer uses bundled IBM Plex Sans and Arabic regular faces, retaining its
existing emoji/unknown-glyph and monospace fallback. Typography sizes come from
the released desktop roles. This binding is not a claim of full shaping or font
coverage for every script. The package's Apache notices, IBM Plex OFL license and
source pin accompany native artifacts; the font notice is also visible in About.

Workspace format 2 adds the theme. Version 1 still loads with its previous dark
default and reading it does not rewrite it. Only explicit Save upgrades to format
2. Older applications do not support format 2; a runtime downgrade must retain
its compatible preference file. Endpoint keys, grants, stored peer references,
OS consent and remote protocol versions are unchanged by appearance preferences.

Native visual checks on Linux and macOS, all four themes, small/large windows,
loading/empty/error/selected/disabled controls and focus behavior are required
before qualifying this renderer binding. Source pin verification and compilation
alone do not establish those user journeys.
