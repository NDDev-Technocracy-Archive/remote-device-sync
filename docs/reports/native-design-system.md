# Native design-system binding checks

The checked runtime source is `280320456564ceb1d52815e694f6cd7d280373fe`.
Its public native producer is
[42491997991](https://github.com/NDDev-Technocracy-OpenNetwork/nddev-remote/actions/runs/42491997991),
CI merge `fc7a4c1526c1b89ceef422588ce55a6cf708706c`, tree
`766a9e5e1dc574ef0817953e9eadc35a87fcda06`. The merge parents and expected
merge tree were independently checked. The unchanged canonical design package,
source pin, notices and complete native package inventories were verified before
execution. All six workflows passed for that runtime head.

The packaged macOS viewer executable SHA-256 is
`e5930dbfdc7565a53e44c12a413ddc87a5caf262afe54d6696adaecfc74b10ae`;
the Linux viewer is
`58b8ebedf5c2573d0b73daabdcc44781e9502adc450cb3846b1327095f9911d3`.
Both Apple Silicon devices passed strict native code-signature verification.
Only temporary viewers were run; resident agents, endpoint identities, stored
user workspaces and OS permissions were preserved.

| Surface | Check and result |
| --- | --- |
| Linux physical GNOME/XWayland | All four themes at 1400 × 850: readable native chrome, correct colors and standalone product identity; clean exit and protected production PIDs unchanged. |
| Native X11/Xvfb, scale 1 | All four themes at 800 × 550 and 1400 × 850: readable empty workspace, selected appearance menu, About and notices, native popup closure, empty connection form and disabled Find displays. |
| Native X11/Xvfb preferences | Four cyclic theme changes through actual pointer input; explicit Save, private mode 0600 and reloaded version-2 appearance verified. Reading/opening a workspace alone did not rewrite it. |
| Native X11/Xvfb connected workspace | Actual loopback QUIC/H.264 pixels, selected display tab, appearance changes, Save, About and display settings. Each of four viewers retained one display session across its theme change. |
| macOS M1 | All four actual empty-workspace themes read back through an admitted screen source; received owned-window crops visually reviewed. Bundled Plex faces, native title and chrome were readable. |
| macOS M2 | Light, dark and dark-contrast current crops reviewed. Cold light-contrast readback remained stale; a separate already-active source captured the correct light-contrast window. |

The connected fixture used synthetic owned pictures and its own loopback manager,
never OS screen capture/input or a production peer. Four viewers produced four
opens and four closes, no remote input, no held keys and a retained fixture
identity. Video continued across appearance changes without reconnecting. This
is renderer/control behavior evidence, not physical end-to-end latency evidence.

Native macOS readback exposes the known installed Host 0.1.1 stale-initial-image
problem in [#206](https://github.com/NDDev-Technocracy-OpenNetwork/nddev-remote/issues/206).
Both first and following crops and their frame headers were retained; a process
exit or a later correct image did not turn the stale sample into a startup pass.
The existing latest-image fix is already in main, but the checked resident Host
had not adopted it. No Host replacement, signing/grant change, input gesture or
sleep was used to declare that defect fixed. The separate active-stream check
qualifies appearance only. Screenshots and device-specific receipts stay private.

Rejected fixture executions are also retained: an early physical small-window
capture preceded actual paint; a harness assertion raced systemd's pre-exec PID;
and the initial Xvfb raw-image check assumed 8-bit components when ImageMagick
returned its default depth. The corrected native fixture waits for painted,
stable owned chrome at explicit 8-bit depth, observes actual executable/window
identity, and checks the saved setting. It changes no application timing or
performance threshold.

Workspace-v1 read-only migration, explicit v2 save/reload, unknown settings,
exact source-file inventory and modified/extra/symlink rejection are covered by
the existing native CLI/package tests. The UI checks above add actual platform
pixels and input. They do not close the separate hidden-window recovery,
impaired-link latency, full clipboard or hardware-codec qualifications in #214.
