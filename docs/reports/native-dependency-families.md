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
pairs. Build-free checks pass. Native compilation and behavior qualification of
the final source remain pending; individual package qualification does not prove
the entire consumer set. No installed runtime, permissions, device pin or build
provider was changed.
