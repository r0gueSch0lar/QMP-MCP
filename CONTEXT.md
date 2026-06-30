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
by name. Kept distinct from the Image Store so install media cannot be written.
_Avoid_: cdrom dir, media pool

**Event Buffer**:
The bounded, server-side ring buffer of recent QMP async events for the current
Instance. The agent reads it pull-style via `get_events`/`wait_for_event`; an optional
push notification is a secondary surface.
_Avoid_: event log, event stream, queue
