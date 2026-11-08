# Native static X11 capture sample

This is an incremental resource check for replacing periodic native capture
polling. It does not qualify hardware encoding, Wayland/macOS resource use,
physical display presentation, or whole-process CPU consumption.

Both executions use the identical ignored native fixture
`session::native_idle_qa::native_static_capture_resource_sample` in a fresh owned
640 × 480 Xvfb server. It constructs the actual X11 capture/producer and OpenH264
encoder, warms the first frame, then requests 20 frames of an unchanged image.
Only the test process receives an explicit X11 environment. The user's desktop,
agent and applications are not restarted or reconfigured.

| Measurement | Main baseline | Event-driven candidate |
| --- | ---: | ---: |
| Measured time, ms | 20,438 | 20,275 |
| Producer calls / nonempty encoded frames | 20 / 20 | 20 / 20 |
| Calling-thread voluntary context switches | 1,991 | 40 |
| Calling-thread involuntary context switches | 107 | 32 |
| Calling-thread CPU ticks | 45 | 26 |

The sample retains the one-second unchanged-picture refresh while removing most
periodic wakeups: voluntary switches fall by 98.0% in this pair. CPU ticks cover
only the calling thread, excluding codec workers. Other authorized workloads ran
on the PC: load averages changed from 10.85 to 21.09 during baseline and from
21.09 to 17.99 during candidate. The tick difference is therefore descriptive,
not a whole-host CPU saving or a controlled performance percentage.

Source baseline: main `c4a66d3a5624e8c8e57724f665d7be4e9f85e3e6`, with only the
same test module and its test-only declaration added. Candidate and baseline
fixture bytes are identical. Candidate source snapshot SHA-256:
`d14405ac83b289c2715a72775bb0f30009596f89a6e3e59c9925ad16f4dcd1c1`;
baseline snapshot: `ec4c45d8bb7dbe34d4853e2491e0a68fede46856da90ddba2d827ec1659b7905`.
Frozen debug test executable SHA-256 values:

- Baseline: `6d5e495803406539b6492c744886b0c4b46e1c70697854668064a49b506ca96a`.
- Candidate: `857b25f1b1a54697790cb7119d546352fbbbcedc2b6603abe0c1e33ccace22c8`.

Both samples exited successfully. They were run sequentially after this task's
builds and native UI captures ended. Raw metadata/logs are retained privately;
the public report contains no device identity, native pixels or clipboard data.

Native checks on the preceding production-identical source snapshot passed:
Linux workspace/all-target denied Clippy, macOS CLI/agent/desktop all-target
denied Clippy, Linux desktop library 224 passed with five pre-existing native
display tests ignored, macOS desktop library 192 passed, and six serving
integration lanes on both platforms. Default-feature Clippy and library tests
also passed (Linux 121, macOS 120). Adding the ignored sample was followed by
Linux all-target denied Clippy and the explicit native executions above. The
shared SDK's unavailable CMake prevents the unrelated full-workspace macOS audio
lane; its evidence remains in controller issue 166 without an SDK bypass.

The first PR CI execution found a fixture error: its all-ignored native suite
uses a different Xvfb extent, whereas the new sample required 640 × 480 during
admission. The sample now takes its extent from the actual first encoded frame
and requires every subsequent frame to retain it. The paired measurements above
remain tied to their original frozen 640 × 480 fixtures and source hashes;
arbitrary-size CI executions are separate checks, not replacement measurements.
After correction, the complete six-test ignored native suite passed on an owned
1280 × 720 Xvfb server; the explicit 640 × 480 sample also passed. Both produced
20 nonempty frames with unchanged extents and cadence. Source snapshot SHA-256:
`b78e3a6a34c77ab7c924c67c5528e282f55db15914fa2e12b499111393bba751`;
test executable: `17aef3d828b2a9f1fd2dbe79a960202f1934ad341fde10cd913dc128287be3ad`.
Denied all-target Linux Clippy passed after this fixture correction.
