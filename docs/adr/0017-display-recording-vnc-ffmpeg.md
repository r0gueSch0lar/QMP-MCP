# Guest display recording: stream the loopback VNC into ffmpeg, capability-gated on an ffmpeg the container bundles and bare metal optionally provides

A Guest's **display** can be recorded to a video file on demand (`start_recording` / `stop_recording`)
whenever the Instance has a `display: vnc` surface (ADR-0010). The server taps QEMU's **own
loopback VNC** — the same RFB source the noVNC Viewer consumes — as a *second, shared* RFB client,
decodes the framebuffer to raw frames, and pipes them into a co-located **ffmpeg** child that
encodes the stream to a file. The feature is **capability-gated on ffmpeg**: present → available,
absent → disabled and fail-closed. This is recorded because the source choice, the hand-rolled
RFB client, and the ffmpeg gate each constrain both implementations and the shipped images.

**Stream the live VNC; do not poll `screendump`.** ffmpeg has **no native VNC/RFB demuxer**
(its capture inputs are `x11grab`/`kmsgrab`/`fbdev`/…), so every path needs a bridge that turns
the framebuffer into something ffmpeg reads. Two families exist. The *polling* family loops QMP
`screendump` (ADR-0015 uses the same QMP surface) and feeds PNG/PPM frames to `image2pipe`; it is
fully **decoupled from the display path** (touches neither the VNC server nor noVNC) but caps out
at a handful of fps for a full-screen capture and produces jerky motion — kept as a **documented
fallback**, not the primary. The *streaming* family taps the live RFB: the recorder connects to
`127.0.0.1:5900`, requests continuous framebuffer updates, and emits raw frames at a target rate.
This wins for fidelity because it rides the exact channel noVNC already renders, so what is
recorded is what the operator sees. The two rejected streaming variants are `Xvfb` + a headless
vncviewer + `ffmpeg -f x11grab` (real-time and ffmpeg-native but drags a whole X stack into the
image) and GStreamer `rfbsrc` (native VNC ingestion but replaces ffmpeg with a second media
toolchain); both are heavier runtime surface than a small RFB client feeding ffmpeg over a pipe.

**The RFB client is hand-rolled, Raw-only, and mirrored in both variants** — the ADR-0011
precedent (the QMP client itself is hand-rolled and kept byte-parity across the TypeScript and
Rust implementations, ADR-0012) applied to a second small protocol. The recorder negotiates the
RFB version, authenticates, reads `ServerInit` (width/height/pixel-format), pins a 32-bit pixel
format with `SetPixelFormat`, and — the deciding simplification — sends `SetEncodings` listing
**Raw only**. QEMU then sends uncompressed rectangles, so neither implementation ever writes a
Hextile/Tight/ZRLE decoder; the cost is uncompressed bandwidth, which is free over loopback. The
client keeps a full-frame buffer, applies each incremental `FramebufferUpdate` rectangle into it,
and — since RFB is change-driven — **emits a frame only when the framebuffer actually changed,
stamped with a real presentation timestamp, capped at a maximum fps** (`QMP_MCP_RECORDING_MAX_FPS`).
It does *not* synthesize duplicate frames for an idle Guest; ffmpeg is fed genuine
variable-frame-rate (`-fps_mode vfr`), so a static screen costs essentially nothing — a long idle
stretch collapses to one frame plus a timestamp gap. This is the single largest space win for
screen capture, and the reason recording is **VFR-on-change, not constant-fps**. ffmpeg reads the
pipe as `-f rawvideo -pix_fmt <server byte order> -s WxH -i pipe:0` and encodes with
**quality-targeted CRF** (not a fixed bitrate — bits are spent only where the screen is busy) plus
the screen-content tuning and long keyframe interval described next. A per-language RFB *library*
was rejected: two
different libraries would reintroduce exactly the cross-implementation divergence ADR-0012 exists
to prevent, and add dependency-maintenance exposure a ~Raw-only client does not.

**Encoding is tuned for screen content and optimized for space-per-quality.** VM output is static,
text-heavy, sharp-edged, and frequently idle, so the encoder is configured for that, not for film:
**CRF (constant quality) rate control**; a **slow preset** (affordable because VM capture is
low-fps and low-resolution, so the encoder has real-time headroom to search harder → smaller files
at equal quality); a **long keyframe interval with scene-cut detection** (`-g` ~30 seconds of
frames, so a static stretch rides one I-frame while a big change still forces a keyframe);
**`yuv420p` 8-bit** by default (4:4:4 only buys colored-text sharpness at a size/compatibility cost
the common case does not need); and the codec's **screen-content tuning** (`libx264 -tune
stillimage`, `libvpx-vp9 -tune-content screen`, `libsvtav1` `scm=2`). The **default codec is
`libx264 -tune stillimage` at CRF ~22** — chosen because it is the one encoder guaranteed present
in every bundled and mainstream distro ffmpeg — with codec, CRF, max-fps, and pixel format all
**operator-overridable** (`QMP_MCP_RECORDING_CODEC` / `_CRF` / `_MAX_FPS` / `_PIXFMT`). Operators
wanting smaller files opt into `libx265`, `libvpx-vp9`, or `libsvtav1` (roughly 30–50% smaller for
this content), which the capability probe confirms are compiled into the build before offering
them. Hardware encoders (NVENC/VAAPI/QSV) are deliberately **not** used: they trade quality-per-byte
for speed, and at VM fps the speed is unneeded while the size penalty is not.

**Authentication and coexistence with the live Viewer.** Because a `display: vnc` Instance arms
the Display password over QMP (ADR-0010), the loopback VNC uses VNC-Authentication, so the
recorder authenticates with the **server-known** `QMP_MCP_VIEWER_PASSWORD` (the classic DES
challenge; the DES itself comes from each platform's crypto library — Rust `des`, Node `crypto` —
never hand-rolled). The recorder connects with the RFB **shared flag set**, and noVNC already
connects shared, so under QEMU's default `allow-exclusive` share policy the recorder and the
Viewer coexist and recording never disconnects the operator's live view — **no argv change** to
force-shared is required (verified against the bundled QEMU before relying on it).

**ffmpeg is capability-gated, container-bundled and bare-metal-optional** — the ADR-0007
"purpose-built image and bare metal are coequal" posture. The container images (`rust/Dockerfile`
and `typescript/Dockerfile`) **add ffmpeg** to the runtime stage alongside `qemu-system` /
`qemu-utils`, so recording is always available in the shipped images. On bare metal ffmpeg stays
an **optional install**: the server resolves it from `QMP_MCP_FFMPEG_BINARY` (explicit override)
else the `PATH`, on **the host running qemu — which is always the server's own host** (the driver
spawns qemu locally, ADR-0013, so there is no remote-emulator case to reach across). Absent →
the recording capability is **off**; `start_recording` fails closed with an actionable message
("recording requires ffmpeg; install it or set `QMP_MCP_FFMPEG_BINARY`"), exactly as KVM
inaccessibility is probed and reported rather than assumed (ADR-0008). Crucially, **the binary
being present is necessary but not sufficient** — the probe also verifies the *selected encoder* is
compiled into that build (`ffmpeg -encoders`), because a distro ffmpeg may ship without
`libx265`/`libsvtav1`/`libvpx`. An operator-pinned encoder that is absent **fails closed** with a
specific message; otherwise recording uses the **`libx264` default, which every bundled and
mainstream distro ffmpeg carries**, so the feature works out of the box in-container and degrades
gracefully on bare metal. A read-only capability surface (a `get_recording`-style report, the shape
`get_serial`/`get_share` established) advertises whether recording is available and why, the
resolved codec and the encoders present in the build, plus the active CRF, max-fps, and pixel format.

**Recording is capture, not input — gated by config, not a second allow-flag.** Unlike
`write_serial`, nothing is typed into the Guest, so there is **no `QMP_MCP_ALLOW_…` gate**; the
trust model mirrors the **spool** serial backend (ADR-0015) instead. Output is written to files
under an **operator-configured root** (`QMP_MCP_RECORDING_DIR`); the agent may name the file only
by the **same Store-name allowlist** used for `serialSpool` (no `/`, `..`, or absolute paths),
resolved under the operator root, so an opt-in recording feature can never become an arbitrary
host-file write. Videos are large, so — unlike `screendump`'s inline PNG — the file is **not
returned inline**; `start_recording` returns an id/path and the operator retrieves the file from
the host. One recording runs per Instance and is **torn down with the Instance** and on an
**unexpected qemu exit**, reusing the instance-scoped teardown discipline the serial socket bridge
established (abort the pump task, close the ffmpeg child so the container is finalized, on destroy
*and* on reconcile). `start_recording` errors actionably when there is no Instance, no `display:
vnc` surface, ffmpeg is unavailable, or a recording is already running.

We record this because the mechanism choices constrain every later increment and must hold
identically across both variants:

1. **Stream the live RFB, keep `screendump`-loop as the documented fallback.** Real-time fidelity
   comes from tapping the same VNC noVNC renders; the polling path stays reachable (and is the only
   option if a deployment ever forbids a second VNC client) but is explicitly not the primary
   because it caps fps and jerks on motion.
2. **Hand-roll a Raw-only RFB client, mirrored, over per-language libraries.** Forcing `Raw`
   removes every compressed decoder, shrinking the client to something a shared ADR-0012 golden
   fixture (a captured Raw update stream both implementations must decode byte-identically, plus
   the ffmpeg-argv and ffmpeg-detection corpora) can pin — which two divergent third-party
   libraries could not.
3. **Gate on ffmpeg *and the selected encoder*; bundle it in the image, make it optional on bare
   metal.** The ADR-0007 coequal posture: the container is always capable, bare metal advertises the
   capability from a local probe (qemu is always co-located, ADR-0013). The probe checks the
   *encoder* is in the build, not just that the binary exists, and defaults to the always-present
   `libx264`, so recording never silently degrades — it works, falls back to the guaranteed codec,
   or fails closed with a specific message.
4. **Capture is operator-config-gated, not injection-gated.** No `QMP_MCP_ALLOW_…` flag, because
   nothing is written to the Guest; the operator owns the host-touching knobs (ffmpeg binary,
   recording root, format, fps) and the agent's entire lever is `start/stop_recording` plus a
   validated filename resolved under the operator root — the spool trust model (ADR-0015), not the
   `write_serial` one.
5. **Coexist with the Viewer and reuse instance-scoped teardown.** The recorder connects shared so
   it never evicts the live noVNC session, and its pump task + ffmpeg child are owned by the
   Instance and released on both destroy and unexpected-exit reconcile — the same lifecycle the
   serial socket bridge uses, so recording cannot leak a process or a half-written file.
6. **Optimize for the static screen: VFR-on-change + CRF, not constant-fps + bitrate.** The pump
   emits a frame only when the framebuffer changes (real PTS, max-fps cap) and ffmpeg encodes VFR
   at quality-targeted CRF with screen-content tuning, a slow preset, and a long keyframe interval —
   so an idle Guest costs almost nothing and busy moments stay crisp. This is the largest space
   lever for screen capture, and it lives in the pump + ffmpeg args (both pinned by the ADR-0012
   fixtures), not in the codec choice, so it holds across every encoder the operator selects.
