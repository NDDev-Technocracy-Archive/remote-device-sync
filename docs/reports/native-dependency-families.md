# Native dependency families

Part of [#210](https://github.com/NDDev-Technocracy-OpenNetwork/nddev-remote/issues/210).
The candidate updates six normally resolved dependencies without changing
manifests, features or patched transport sources:

| Package | Previous | Candidate |
| --- | --- | --- |
| enum-assoc | 1.4.1 | 1.4.2 |
| futures-buffered | 0.2.13 | 0.2.14 |
| opusic-sys | 0.7.5 | 0.7.7 |
| ordered-float | 5.5.0 | 5.5.1 |
| plist | 1.10.1 | 1.10.3 |
| zcheapstr | 1.1.0 | 1.2.0 |

All six exact official package records are non-yanked, older than three days,
and match the resolved source checksums. The Opus change affects the native
codec used by `rds-audio`; existing real packet/PCM-format, jitter and corruption
cases remain required. Interface, XML and netlink API families retain the
versions their actual parent ranges require. Resolver changes also reselect
six existing Windows-only dependency edges from windows-sys 0.61.2 to the
already resolved 0.52.0; this does not establish Windows support.

The enabled native graphs contain 567 Linux and 510 Mac external package/version
pairs. Build-free checks and all six workflows pass for source
`555a19c9dbe532145398cd7f9a7252206cd07edc`, merged by PR #240. Native run
42583768154 checks the same tree, `4674bdd865ae1c7de9e9fd708786b2553c12dfe9`:
Linux passes 241 desktop cases and two real-agent handshakes, Mac passes 208
and two. Both execute all 25 audio cases, including real Opus packets and PCM
formats. Platform-dependent exclusions remain explicit.

Artifact 13658823520 supplies the optimized AppKit probe
`594ee2bf4672bca0e265c77a488047396afcf2751dfc98a06d263c1aa3653109`.
Strict signature verification and all 18 isolated native cases pass on both
physical Apple Silicon devices. Both logs retain the
`sandbox_extension_consume failed: 1` warning. These native-owner checks do not
establish receiving-application clipboard success. No installed runtime,
permissions, device pin or build provider was changed.
