# QEMU MCP Server

An MCP server that lets an AI agent build, launch, drive, and tear down a single
QEMU virtual machine through QEMU's QMP API. The agent composes the machine's
hardware, the server runs it and bridges agent requests to QMP.

## Language

**Instance**:
The single managed QEMU virtual machine the server controls at a given time — the
`qemu-system-*` process together with its hardware configuration and its live QMP
connection. Only one Instance exists at a time.
_Avoid_: VM, machine, domain (libvirt's term)

**Guest**:
The operating system or workload running *inside* the Instance.
_Avoid_: VM, target

**QMP Session**:
The negotiated QMP connection to a running Instance — established after the server
reads the greeting and sends `qmp_capabilities` to leave negotiation mode.
_Avoid_: monitor connection, control channel

**Hardware Spec**:
The structured, validated description of an Instance's hardware (machine type, CPU,
vCPUs, memory, disks, NICs, display, boot order, accelerator). The server generates
the `qemu-system-*` argv from it; the agent never supplies raw argv.
_Avoid_: machine config, VM definition, profile

**extraArgs**:
The opt-in escape hatch for appending raw QEMU arguments to a generated argv.
Disabled unless explicitly enabled by an env flag; meant for trusted single-tenant use.
_Avoid_: passthrough, custom args

**Command Policy**:
The allow/deny configuration that governs which QMP commands the generic execute
tool may run. Defaults to a safe allowlist; dangerous commands (e.g.
`human-monitor-command`, `migrate`, `dump-guest-memory`) sit behind a hard denylist
and an env-gated opt-in.
_Avoid_: whitelist, filter, ACL

**Image Store**:
The single configured, read-write directory that holds guest disk images. Disks are
referenced by name within it (never by host path) and new blank images may be created
inside it. The boundary that keeps storage selection off the host filesystem.
_Avoid_: disk pool, volume dir

**ISO Store**:
The separate, read-only directory that holds installation/boot ISO media, referenced
by name. Read-only on the agent's side — kept distinct from the Image Store so install
media cannot be written by a Guest — with one server-side writer: `download_iso`
fetching Download Catalog entries into it, behind the operator's
`QMP_MCP_ALLOW_DOWNLOAD` gate (ADR-0018).
_Avoid_: cdrom dir, media pool

**Download Catalog**:
The operator-curated list of OS installation images `download_iso` may fetch into the
ISO Store (ADR-0018). Data, not code: a bundled JSON list (kept byte-identical across
both variants by a parity test), replaceable via `QMP_MCP_ISO_CATALOG`. The agent
selects an entry by its short `id` — it never supplies a URL — and downloading stays
disabled until the operator opts in.
_Avoid_: mirror list, app store, image registry

**Event Buffer**:
The bounded, server-side ring buffer of recent QMP async events for the current
Instance. The agent reads it pull-style via `get_events`/`wait_for_event`; an optional
push notification is a secondary surface.
_Avoid_: event log, event stream, queue

**Display**:
The Guest's graphical output, exposed by QEMU itself over VNC and selected through the
Hardware Spec. Portable: it is a plain QEMU feature, so it works identically on bare
metal and in any image.
_Avoid_: screen, console, framebuffer

**Viewer**:
The optional noVNC browser bridge over a Guest's VNC Display, started and stopped by
the server for the lifetime of a `display: vnc` Instance. Reads the Display only —
never the QMP Session — and is fail-closed behind its own password.
_Avoid_: console, GUI, web UI, VNC server

**Recording**:
The on-demand capture of a `display: vnc` Guest's Display to a video file
(`start_recording` / `stop_recording` / `get_recording`, ADR-0017): a screendump loop
piped into a co-located ffmpeg. Capability-gated, not permission-gated — it is capture,
not input — available only when ffmpeg is runnable, the selected codec is compiled into
it, and the operator set `QMP_MCP_RECORDING_DIR`. The agent only *names* the output,
saved as `<name>.mkv` under that operator root and never returned inline. One Recording
runs per Instance and is finalized on destroy or unexpected exit.
_Avoid_: screencast, screen capture (as a term), video (bare)

**Serial Port**:
The Guest's emulated serial line, attached on demand (`serial: true`). One of three
operator-selected backends (`QMP_MCP_SERIAL_BACKEND`) carries it: **ringbuf** — a
bounded QEMU `ringbuf` chardev the server drains over QMP with `ringbuf-read` (ephemeral;
deliberately QEMU's own buffer, not a server-side one — contrast the Event Buffer);
**spool** — a host file under the operator's `QMP_MCP_SERIAL_SPOOL_DIR` root, placed in a
per-Instance subdirectory the spec may name (validated like an Image/ISO Store name, never a
raw host path); or **socket** — a server-owned UNIX socket bridged onto the line (ADR-0015),
the interactive backend: output drains from a bounded in-server ring and `write_serial`
reaches the Guest as a real connected serial line. Its content is the Guest's serial console
output; read-only by default, writable (ringbuf and socket) behind an operator env gate.
Portable: a plain QEMU feature.
_Avoid_: serial console (as a standalone term), COM port, tty, ring buffer (that is the Event Buffer)
