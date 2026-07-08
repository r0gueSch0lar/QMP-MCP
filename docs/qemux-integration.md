# Design note: extending `qemux/qemu` as a future QEMU runtime

Status: exploratory. Deferred from [ADR-0007](./adr/0007-purpose-built-image-baremetal-coequal.md),
which records that we ship a purpose-built slim image today and do **not** extend the
[`qemux/qemu`](https://github.com/qemus/qemu) community image. This note examines whether
`qemux/qemu` could become an *alternative* base for the container build, what it would buy
us, and why its design collides with ours. It is a design note, not a commitment.

Domain terms (Instance, Guest, QMP Session, Hardware Spec, Image Store, ISO Store, Command
Policy, Event Buffer) are defined in [`CONTEXT.md`](../CONTEXT.md).

## 1. What `qemux/qemu` offers today

Grounded in the project README (<https://github.com/qemus/qemu>), its Docker Hub page
(<https://hub.docker.com/r/qemux/qemu>), and the repo `Dockerfile`
(`raw.githubusercontent.com/qemus/qemu/master/Dockerfile`), fetched 2026-07-01.

`qemux/qemu` is a "QEMU in a container" appliance: you give it an OS and it boots that OS
as a VM, exposing a browser viewer so you can watch and drive the install. Confirmed facts:

- **Env-var VM config.** The VM is described entirely by environment variables — `BOOT`
  (an OS keyword like `ubuntu`/`fedora`, or a URL to a disk/ISO), `RAM_SIZE` (default
  `2G`), `CPU_CORES` (default `2`), `DISK_SIZE` (default 64 GB), `DISK_TYPE`
  (`virtio-scsi` default, `blk`, `ide`), `BOOT_MODE` (`legacy` to drop UEFI),
  `DISK2_SIZE`/`DISK3_SIZE` for extra disks, `ARGUMENTS` to append raw QEMU args, and
  `DEBUG=Y` to print the assembled QEMU command line.
- **Auto-boot entrypoint.** `ENTRYPOINT ["/usr/bin/tini", "-s", "/run/entry.sh"]`. On
  start the container reads those env vars, fetches/prepares the boot media, sizes the
  disk, sets up networking, and **boots its own VM** — the user just opens the viewer.
- **Browser viewer.** `EXPOSE 22 5900 8006`. The Dockerfile pulls **noVNC** plus
  `websocketd`, so the web viewer on **port 8006** is noVNC bridging to the guest's VNC
  server on `5900`; `22` is for SSH into the guest.
- **Tuned KVM + networking.** Documented to use `/dev/kvm` for acceleration and
  `/dev/net/tun` + `cap_add: NET_ADMIN` for networking. The Dockerfile installs **passt**
  (`qemus/passt`) for user-mode networking, and the docs describe several modes:
  host-bridge (default, shares the host IP), per-port forwards, `USER_PORTS` for
  user-mode exposure, **macvlan** for a dedicated guest IP, and `DHCP=Y` (with
  `/dev/vhost-net`) to pull an address from the LAN router. The README suggests
  `privileged: true` as a troubleshooting step when KVM/networking detection fails.
- **Multi-format disk support.** Accepts `.iso`, `.img`/`.raw`, `.qcow2`, `.vmdk`,
  `.vhd`, `.vhdx`, `.vdi`, and auto-extracts compressed variants (`.qcow2.xz`,
  `.img.gz`, `.iso.zip`, …).
- **Base / packages.** `FROM debian:trixie-slim`, installing `qemu-system-x86`,
  `qemu-utils`, `websocketd`, noVNC, passt, and `python3` with the `qemu.qmp` module —
  i.e. its entrypoint scripts already speak **QMP** to the VM they boot.

**Could not confirm (do not assume):** the exact pinned QEMU version (it tracks Debian
trixie's `qemu-system-x86` package; no explicit version in the docs we read); and an
explicit non-root `USER` — none is documented, and the entrypoint's work (disk resize,
`tini` as PID 1, network setup, optional `privileged`/`NET_ADMIN`) strongly implies it
runs as **root**. Treat the runtime UID as root until proven otherwise.

## 2. The core conflict: two entrypoints, two QMP owners

`qemux/qemu` and `qmp-mcp` are both *the thing that owns the VM*, and a container has
exactly one PID 1. That is the collision.

- **qmp-mcp is the entrypoint and owns the Instance lifecycle.** Per
  [ADR-0001](./adr/0001-single-instance-lifecycle-orchestrator.md), our server *is* the
  orchestrator: it takes a validated Hardware Spec, **generates the `qemu-system-*`
  argv** (no raw args; ADR-0002), spawns the `qemu-system` process itself, negotiates the
  **QMP Session** (reads the greeting, sends `qmp_capabilities`), and drives the Guest
  through that socket — build → run → QMP → destroy. Our `ENTRYPOINT` is
  `node dist/index.js`, deliberately *not* a VM-booting wrapper (ADR-0007).
- **qemux's entrypoint already booted *its* VM and owns *its* QMP.** `/run/entry.sh`
  consumes `BOOT`/`RAM_SIZE`/`CPU_CORES`/… and starts a `qemu-system` process before any
  of our code could run. It even ships `python3 qemu.qmp` so its own scripts hold a QMP
  channel to that VM for the viewer/lifecycle.

The two cannot coexist as-is:

1. **Configuration ownership.** qemux derives hardware from *its* env vars; qmp-mcp
   derives it from a per-request **Hardware Spec** that the agent fills at runtime. There
   is no single boot-time env that expresses "a VM the agent has not described yet." Our
   Instance is created on a `create_instance` call, not at container start.
2. **Process & QMP ownership.** If qemux boots a VM, that QEMU process and its QMP socket
   belong to qemux's scripts, not to our orchestrator. qmp-mcp would have nothing to spawn
   and no greeting to negotiate; it cannot manage an Instance it did not launch. (Bridging
   to an externally-started QEMU is the explicit non-goal of ADR-0001.)
3. **Lifecycle & cardinality.** qemux boots one VM at start and keeps it up; qmp-mcp
   creates and destroys Instances on demand and is `NONE` until asked. Letting qemux boot
   first means a VM we never asked for, sized by env we did not validate.
4. **Posture.** qemux leans on `NET_ADMIN`, `/dev/net/tun`, bridge/macvlan/DHCP, and
   "try `privileged`," likely as root. qmp-mcp is **non-root, never `--privileged`**, KVM
   is an opt-in `/dev/kvm` upgrade (ADR-0008), and networking is **user-mode/SLiRP by
   default with host networking env-gated off** (ADR-0009). Adopting qemux's defaults
   would regress that posture.

In short: qemux is built to *be the appliance*; qmp-mcp is built to *be the orchestrator*.
You cannot have both entrypoints boot a VM.

## 3. Integration sketch: qemux as a runtime provider only

The only viable use of qemux is as a **QEMU-binary-and-tuning provider**, with its
appliance behavior fully suppressed — never as a parent we boot under. Concretely:

- **Override the ENTRYPOINT.** Build `FROM qemux/qemu` (or copy its qemu binaries) and set
  `ENTRYPOINT ["node", "dist/index.js"]`, so `/run/entry.sh` **never runs**. We keep our
  multi-stage build: compile in the Node stage, copy `dist/` into a qemux-derived runtime
  stage. qmp-mcp remains PID 1 and the sole spawner of `qemu-system`.
- **Inherit (what we'd gain):** the `qemu-system-x86`/`qemu-utils` binaries, passt for
  user-mode networking, and — *optionally* — the noVNC + `websocketd` viewer stack as a
  way to surface the Guest display beyond our `screendump` tool. We would point our
  generated argv at a VNC display and run noVNC/websockets as a *sidecar* the server
  starts, not via qemux's entrypoint.
- **Fight / discard (what we'd lose or have to neutralise):** the entire env-var VM
  contract (`BOOT`, `RAM_SIZE`, `DISK_SIZE`, …) is dead weight — our Hardware Spec is the
  source of truth, so those vars must be ignored, not merely unset. The bridge/macvlan/
  DHCP defaults and the `NET_ADMIN`/`/dev/net/tun`/`privileged` expectations must be kept
  off to preserve ADR-0008/0009. We inherit qemux's Python/qmp.qmp, websocketd, and
  download tooling as unused **attack surface** unless we prune it.
- **Where the models must line up:**
  - **QMP socket.** Our orchestrator owns the only QMP Session. Any viewer/sidecar reads
    the Guest *display* (VNC), and must not become a second QMP client racing our Session
    or bypassing the **Command Policy** (ADR-0003) and **Event Buffer** (ADR-0001).
  - **Non-root.** qemux appears to run as root; we must add a dedicated non-root user and
    `USER` it (as today's Dockerfile does), or the ADR-0008 posture regresses.
  - **Store mounts.** Disks/ISOs still resolve **by name** inside the read-write **Image
    Store** (`QMP_MCP_IMAGE_DIR`) and read-only **ISO Store** (`QMP_MCP_ISO_DIR`) per
    ADR-0006. qemux's `BOOT`-fetches-a-URL model is the opposite of our allowlist and must
    stay disabled; the stores are mounted exactly as they are now, owned by our user.
  - **Bare metal stays co-equal.** Per ADR-0007 the server must still run via `npx`/`node`
    on a host with QEMU on `PATH`. Any qemux-only feature (its viewer, passt tuning) has
    to remain a *container nicety*, never a hard dependency, or it breaks the bare-metal
    target.

Net: this is feasible but inverts qemux's intent — we would import a few binaries and one
optional viewer while disabling the 90% of qemux that is the appliance. The "fight"
surface (entrypoint, env contract, networking defaults, root) is larger than the "inherit"
surface.

## 4. Trade-offs vs the purpose-built slim image (ADR-0007)

| Axis | Purpose-built slim (today) | qemux-derived |
| --- | --- | --- |
| Image size / leanness | `debian:bookworm-slim` + `qemu-system` (all archs, so any guest the spec supports), `qemu-utils`, Node, prod deps — no unused runtime cruft. | Heavier: also noVNC, websockets, passt, Python + `qemu.qmp`, qemux scripts — much of it unused if the entrypoint is overridden. |
| Control / reproducibility | We own every layer and the `ENTRYPOINT`; build is deterministic from our lockfile + apt. | We inherit upstream's layers, scripts, and update cadence; our build tracks an external image. |
| Version pinning | We pin Debian + the qemu packages directly. | QEMU version is whatever qemux/Debian trixie ships; pinning means pinning an upstream tag and trusting its contents. |
| Attack surface | Only what the server launches and probes. Non-root, no `NET_ADMIN`, user-mode net. | Adds a network-listening viewer (8006), websockets, an interpreter, download/fetch tooling, and root-leaning defaults — all to suppress, not use. |
| Maintenance | We track Debian + qemu CVEs ourselves. | Upstream may move fast or break our overrides (entrypoint, USER, networking); we'd re-verify the suppression on each bump. |
| What we gain | — | Battle-tested KVM/networking tuning (passt) and a ready browser viewer. |
| What we lose | — | Entrypoint control, leanness, and a clean non-root/user-mode posture unless we re-impose all of it. |

**Recommendation: stay purpose-built for now.** The slim image already delivers what
qmp-mcp needs — `qemu-system-x86` + `qemu-utils`, non-root, KVM-optional (ADR-0008),
user-mode networking (ADR-0009) — with full `ENTRYPOINT` control and a small, auditable
surface. Deriving from qemux would mean importing a large appliance mostly to switch it
off, trading leanness and posture for a viewer we can otherwise add cheaply (noVNC as a
small, optional sidecar over the Guest's VNC display) and tuning we do not yet need. The
collision in section 2 is structural, not cosmetic: qemux wants to be the entrypoint, and
that is the one thing ADR-0001/0007 reserve for the orchestrator.

## When to revisit

Revisit a qemux-derived build if **any** of these become true:

- **The browser viewer becomes a first-class requirement** and we want a maintained noVNC
  stack rather than building/maintaining our own sidecar.
- **We hit QEMU networking limits** (e.g. SLiRP/user-mode performance or feature gaps) and
  qemux's passt/bridge/macvlan tuning would materially help — *and* we are ready to relax
  the non-root/host-networking posture under an explicit opt-in (ADR-0008/0009).
- **qemux exposes a clean "binaries-only" / "no-boot" mode** (a documented way to disable
  `/run/entry.sh` and the env-var VM contract upstream), removing most of the "fight"
  surface in section 3.
- **Maintaining our own QEMU packaging becomes a real burden** (e.g. tracking QEMU CVEs or
  multi-arch builds) and inheriting upstream's cadence is a net win.

Until then, the purpose-built slim image (ADR-0007) remains the shipping target, and this
note stays a record of the option, not a plan to adopt it.
