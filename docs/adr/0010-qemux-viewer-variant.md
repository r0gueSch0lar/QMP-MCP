# Optional noVNC browser Viewer as a server-managed sidecar in the purpose-built image

We add an optional browser **Viewer** (noVNC) to the existing purpose-built slim
image — we do **not** extend `qemux/qemu`. This **affirms**
[ADR-0007](./0007-purpose-built-image-baremetal-coequal.md) rather than amending it:
we stay purpose-built and keep full entrypoint/posture control, and get the viewer the
cheap way the [`qemux-integration.md`](../qemux-integration.md) note recommended
("noVNC as a small, optional sidecar over the Guest's VNC display"), instead of
importing an appliance whose entrypoint, root, and networking defaults we'd only fight.

**Display vs Viewer.** A **Display** is the Guest's VNC output: a Hardware Spec field
(`display: none` default, or `vnc`) that generates `-vnc 127.0.0.1:N` with a
server-set password. It is a portable QEMU feature, so it works on bare metal too. A
**Viewer** is the noVNC browser bridge over that Display; the orchestrator starts and
stops it as an **Instance-lifetime** concern (server-managed, ADR-Q3 option A), so it
exists only while a `display: vnc` Instance runs. The bridge is **in-process Node**: the
server serves the noVNC static app (shipped via the `@novnc/novnc` npm dependency) and
proxies a websocket to the loopback VNC TCP port using `ws` + `node:net` — no Python and
no external binary. Because it ships in the npm package, the Viewer works identically on
bare metal (`npx`) and in the container, and the Dockerfile only needs to `EXPOSE` the
viewer port. Owning the HTTP layer ourselves makes `QMP_MCP_VIEWER_PASSWORD` a real
fail-closed gate in front of the page and the websocket, with the server-set VNC password
as a second layer behind it.

**Security.** VNC binds **loopback only** (the bridge is the sole VNC client; 5900 is
never published). The Viewer is **fail-closed**: it refuses to serve unless a dedicated
`QMP_MCP_VIEWER_PASSWORD` is configured — distinct from the MCP `API_KEYS` because the
audience is a human in a browser, and noVNC is an interactive keyboard/mouse control
surface, not a passive screen. `display` defaults to `none`, so the Viewer surface does
not exist unless explicitly requested and authenticated. HTTP Basic carries a username
too, which is ignored by default (the password is the secret); setting the optional
`QMP_MCP_VIEWER_USER` additionally enforces that username (constant-time compared,
alongside the password) for operators who want a two-part credential.

**Bare metal stays co-equal** (ADR-0007): the Display is a portable QEMU feature, and
the Viewer is designed to work off it without importing qemux.

We chose to add noVNC ourselves rather than derive from qemux because the browser
viewer is the only qemux capability we wanted, and a small self-owned sidecar over a
portable VNC Display delivers it while keeping the image lean, non-root, and fully
under our entrypoint control.
