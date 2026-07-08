# qmp-mcp

[![CI](https://github.com/r0gueSch0lar/QMP-MCP/actions/workflows/ci.yml/badge.svg)](https://github.com/r0gueSch0lar/QMP-MCP/actions/workflows/ci.yml)

**qmp-mcp** is a [Model Context Protocol](https://modelcontextprotocol.io) (MCP) server
that gives an AI agent the controls of a single [QEMU](https://www.qemu.org) virtual
machine. The agent describes the hardware it wants; the server builds that machine, boots
it, and exposes a set of tools to drive it — pause and resume it, reset it, watch its
screen, send it low-level QEMU commands, react to its events, and tear it down when it's
finished.

The whole design rests on one idea: the *tools are the boundary*. The agent never hands
raw arguments to QEMU or reaches into your filesystem. It fills in a structured, validated
description of the machine; the server turns that into a locked-down QEMU command line and
mediates every request. Everything the agent can touch — disk images, boot media, the
commands it can run against the live VM, the ports it can open — passes through allowlists
you control. The VM is the blast radius, and the tools are the walls.

It ships as **two interchangeable implementations** — one in
**[TypeScript](typescript/)**, one in **[Rust](rust/)** — that behave identically. This
page explains what the server *is* and how it thinks; the per-implementation READMEs cover
installing, running, and deploying each one.

> New to the vocabulary? [`CONTEXT.md`](CONTEXT.md) is the one-page glossary. The words
> below — *Instance*, *Guest*, *Hardware Spec*, *Command Policy*, *Image Store*, *Viewer*
> — each mean something specific, and this README uses them deliberately.

## How it works

### One machine at a time: the Instance

The server manages exactly one **Instance** — the running `qemu-system-*` process together
with its hardware configuration and the live control connection to it. There's never more
than one; asking to create another while one exists is refused. An Instance's life is tied
to the server's: shut the server down and it tears the VM down with it, so nothing is left
orphaned.

An Instance moves through a small lifecycle — from nothing, to starting, to **running**,
optionally **paused** and back, to stopped, and back to nothing:

```
NONE → STARTING → RUNNING ⇄ PAUSED → STOPPED → NONE
```

If the underlying QEMU process exits on its own — a guest shutdown, a crash, an external
kill — the server notices and reconciles back to `NONE`, so the next request starts from a
clean slate.

The thing running *inside* the Instance — the operating system or workload — is the
**Guest**. The server manages the machine; what you install and run on it is up to you and
your agent.

### Describing the machine: the Hardware Spec

The agent doesn't run QEMU. It submits a **Hardware Spec** — a structured, validated
description of the machine it wants: machine type and CPU, how many vCPUs and how much
memory, which disks and boot media, the network, the display, the accelerator. The server
validates every field and *generates* the QEMU command line from it. The agent never
supplies raw argv.

A spec is just the JSON arguments to `create_instance`:

```json
{
  "machine": "q35",
  "cpu": "host",
  "vcpus": 2,
  "memoryMb": 2048,
  "accel": "auto",
  "disks": [{ "image": "root.qcow2" }],
  "cdrom": { "iso": "debian-13.iso" },
  "boot": "dc",
  "display": "vnc"
}
```

Validation isn't a formality — it's the safety boundary. Fields are range- and
character-checked, and anything that could smuggle an extra option into the command line (a
stray comma in a disk entry, say) is escaped or rejected. Sizes are capped, with ceilings
you set on disk, memory, and vCPUs. If a spec is invalid, `create_instance` fails *before*
QEMU is launched, with a message that says exactly what was wrong.

There is an escape hatch — **extraArgs**, which appends raw QEMU flags to the generated
command line — but it's off unless you explicitly enable it. It's meant for trusted,
single-tenant setups where you've decided the agent can be handed the keys.

Which architecture you emulate falls out of the `machine`: the server picks the emulator
for you — `q35`/`pc` launch `qemu-system-x86_64`, while `virt` and the `raspi*` boards
launch `qemu-system-aarch64` — so switching architectures is just a different `machine`,
no restart. `QMP_MCP_QEMU_BINARY` overrides that choice for every Instance (e.g. a custom
build or `qemu-system-riscv64`), and `accel: auto` only uses KVM when the guest arch
matches the host, falling back to TCG across architectures (ADR-0013).

Some machines don't boot from a disk at all. QEMU's Raspberry Pi boards (`raspi3b` and
friends) have fixed hardware — a set CPU, core count, and RAM — and they expect the kernel
handed to them directly rather than read off an SD-card bootloader. For those the spec grows
three optional fields: **`kernel`** and **`dtb`** (a kernel image and device-tree blob, each
a name in the Image Store) and **`appendCmdline`** (the kernel command line). The server
emits `-kernel`/`-dtb`/`-append` and, because the board's hardware is fixed, omits
`-cpu`/`-smp`/`-m`; attach the SD image with `"interface": "sd"` (sized to a power of two, or
QEMU refuses it). These boards also have no PCI bus, so the default NIC can't attach — pick
`network.model` `usb-net` (their USB NIC) or `network.mode` `none`; the server refuses an
unattachable NIC up front rather than letting QEMU abort. None of this is Pi-only — any
direct-kernel boot (a bare `virt` machine, say) can use `kernel`/`appendCmdline` alongside the
usual CPU and memory settings.

### How fast it runs: the accelerator

`accel: "auto"` (the default) uses hardware **KVM** when the host can reach a `/dev/kvm`,
and otherwise falls back to **TCG** software emulation — reporting which it chose. Ask for
`kvm` explicitly and it fails loudly if KVM isn't available; ask for `tcg` and you always
get portable, zero-privilege emulation. KVM is never required — it's a performance upgrade
you opt into, not a privilege the server demands.

### Driving the running VM: the QMP Session

Once an Instance is up, the server talks to it over the **QMP Session** — QEMU's own
Machine Protocol, a JSON control channel on a private socket the server owns and never
exposes on the network. The server negotiates the session at launch (reads the greeting,
sends `qmp_capabilities`), and from then on every "drive the VM" tool is a QMP command
underneath: `pause_instance` stops the CPUs, `get_status` asks QEMU its run state,
`screendump` grabs a framebuffer snapshot, and so on.

For anything without a purpose-built tool, there's `qmp_execute` — a generic "run this QMP
command" — which brings us to the guardrail on it.

### What the agent may command: the Command Policy

`qmp_execute` could in principle run *any* QMP command, which is both powerful and
dangerous. The **Command Policy** decides which ones actually go through. Out of the box
it's a safe-by-default allowlist; genuinely dangerous commands — `migrate`,
`dump-guest-memory`, `human-monitor-command`, and their kin — sit behind a **hard denylist
that can't be re-enabled**. You can widen or narrow the middle ground with an environment
variable or a policy file.

One subtlety: the policy gates commands by *name*, not by their arguments. So a command
whose *arguments* could be dangerous — a screen capture that writes to a host file, for
instance — isn't exposed through the generic tool at all. It gets a purpose-built tool that
validates the arguments for you.

### Where files live: the Image Store and ISO Store

The agent refers to disks and boot media *by name*, never by host path — and those names
resolve inside two folders you designate:

- The **Image Store** is a single read-write directory for guest disk images. The agent
  can list what's there and create new blank images in it, and disks in a spec are looked
  up by name within it.
- The **ISO Store** is a separate read-only directory for installation and boot ISOs.
  Keeping it distinct means install media can never be written to.

Both are enforced with real-path containment: a name that tries to climb out — `../`, an
absolute path, a symlink pointing elsewhere — is refused. These two folders *are* the
agent's view of the filesystem; it has no other.

### Sandboxed networking

Guests get user-mode networking by default — a sandboxed NAT stack, no host privileges, no
bridge. To reach a service inside the guest you add **host forwards**, and those are
bounded: only ports in a non-privileged range, bound to loopback.

```json
{ "network": { "hostForwards": [{ "hostPort": 2222, "guestPort": 22 }] } }
```

Host-level networking (`tap`/`bridge`) exists but is gated off unless you turn it on — it
needs privileges that don't fit the server's unprivileged posture.

### Watching what happens: events, the Display, and the Viewer

Two ways to see what the VM is doing:

- **Events.** QEMU emits async events — a reset, a shutdown, a device change. The server
  keeps a bounded ring buffer of the recent ones for the current Instance, and the agent
  reads it pull-style: `get_events` drains what's new since a cursor, `wait_for_event`
  blocks until a named event arrives (or times out). No firehose to manage.
- **The Display and the Viewer.** Ask for a `vnc` **Display** in the spec and QEMU exposes
  the guest's screen over VNC, on loopback only. Turn on the **Viewer** — an optional,
  in-process noVNC bridge — and you can watch and control that screen in a browser. The
  Viewer is password-gated and reads the Display *only*; it never touches the QMP Session.
  It's ideal for babysitting an OS installer, or just seeing what the agent sees. Most
  machines (`virt`, `q35`, …) have no built-in display, so pair `display: vnc` with a
  **`displayDevice`** — `virtio-gpu` (a real GPU with DRM, so Wayland/X desktops render),
  `vga`, or `ramfb`. Use `vga` for a **live ISO** or any boot where the boot menu / early
  console must be visible: `virtio-gpu` shows nothing until the guest loads its DRM
  driver, so an ISO's bootloader can't draw on it. The `raspi*` boards render over their
  built-in framebuffer, so they stay `displayDevice: none`. (Booting a distro this way
  also takes `initrd` alongside `kernel` — the usual kernel + initramfs + rootfs.)

### Talking to the server: transports and authentication

The server speaks MCP over **stdio** (the default — how most clients launch a server
directly; no network, no auth) or over **HTTP** (for a networked deployment), or both at
once. The HTTP transport is **fail-closed**: it refuses to start without authentication —
an API key, or a signed HS256 token — unless you explicitly opt into insecure mode for
local use. A server that can build and run VMs has no business being reachable
unauthenticated. It runs as a non-root user in every mode and never needs `--privileged`.

## The tools

The agent's vocabulary — the actions it can take:

| Tool | What it does |
| --- | --- |
| `create_instance` / `destroy_instance` | build & launch the Instance from a Hardware Spec / tear it down |
| `get_instance` / `get_status` | the current Instance + lifecycle state / the live guest run state |
| `pause_instance` / `resume_instance` | freeze / unfreeze the guest CPUs |
| `reset_instance` / `powerdown_instance` | hard reset / request a graceful ACPI shutdown |
| `list_block_devices` / `query_cpus` | the VM's disks & backing media / per-CPU info |
| `screendump` | a PNG screenshot of the Display |
| `get_events` / `wait_for_event` | recent QEMU events / block until a named one arrives |
| `qmp_execute` | a raw QMP command, gated by the Command Policy |
| `create_image` / `list_images` / `list_isos` | make a disk image / list disks / list boot ISOs |

For the exact per-implementation tool tables, see the
[TypeScript](typescript/README.md#the-tools) and [Rust](rust/README.md#the-tools) READMEs.

## Quick start: common scenarios

The server runs wherever QEMU is installed. First get one of the implementations running
and point your MCP client at it —
**[run the TypeScript variant](typescript/README.md#run-it)** or
**[run the Rust variant](rust/README.md#run-it)** — then ask your agent to do something.
The scenarios below are what that looks like: each is a Hardware Spec (the arguments to
`create_instance`) plus whatever you had to put in place first.

### 1. A scratch VM to poke at

Nothing to set up — just ask for a small machine and drive it.

> *"Boot a 1 GB Linux VM and tell me its run state."*

The agent calls `create_instance` with a minimal spec, then `get_status`; `destroy_instance`
cleans up:

```json
{ "machine": "q35", "cpu": "host", "vcpus": 1, "memoryMb": 1024, "accel": "auto" }
```

(With no disk or ISO there's nothing to boot — perfect for a smoke test; add media for the
real thing.)

### 2. Install an OS from an ISO

Put the installer ISO in your **ISO Store** folder; the agent creates a blank disk for it
and boots from the CD first (`boot: "dc"`).

> *"Create a 20 GB disk and install Debian from debian-13.iso onto it."*

It calls `create_image` (into the Image Store), then `create_instance`:

```json
{
  "machine": "q35", "cpu": "host", "vcpus": 2, "memoryMb": 2048, "accel": "auto",
  "disks": [{ "image": "debian.qcow2" }],
  "cdrom": { "iso": "debian-13.iso" },
  "boot": "dc",
  "display": "vnc"
}
```

Because it asked for `display: "vnc"`, you can watch the installer run — see scenario 4.

### 3. A headless server you can SSH into

Add a **host forward** so a port on your host reaches a port in the guest.

> *"Run my server image headless and forward host port 2222 to guest 22."*

```json
{
  "machine": "q35", "cpu": "host", "vcpus": 2, "memoryMb": 2048, "accel": "auto",
  "disks": [{ "image": "server.qcow2" }],
  "network": { "hostForwards": [{ "hostPort": 2222, "guestPort": 22 }] }
}
```

Once it's booted, `ssh -p 2222 user@localhost` from the host reaches the guest's SSH.

### 4. Watch it in a browser

Set `QMP_MCP_VIEWER_PASSWORD`, ask for a `vnc` display, and open the Viewer. The setup
details are in the [TypeScript](typescript/README.md#browser-viewer) /
[Rust](rust/README.md#browser-viewer) READMEs; any spec with `"display": "vnc"` then gets a
live, interactive screen at `http://<host>:6080/`.

### 5. Emulate a different architecture

Pick an ARM machine and CPU — the `qemu-system-aarch64` emulator is chosen automatically
from the `machine` (no `QMP_MCP_QEMU_BINARY` needed).

> *"Bring up an ARM64 virtual machine."*

```json
{ "machine": "virt", "cpu": "cortex-a72", "vcpus": 2, "memoryMb": 2048, "accel": "tcg" }
```

On an x86 host `accel: auto` already resolves to TCG (an aarch64 guest can't use x86
KVM). On an ARM host it would use KVM, which only accepts a `host`/`max` CPU — so a named
model like `cortex-a72` there needs `accel: tcg` (as above), and the `raspi*` boards
always run under TCG (their baked CPU can't be virtualized).

(If you also need to *build* the Rust binary for a non-x86 host, see its
[cross-compilation guide](rust/README.md#building-for-other-platforms).)

### 6. Emulate a Raspberry Pi board

QEMU's Raspberry Pi machines boot a kernel directly and render a framebuffer you can watch
in the browser Viewer. Put the extracted kernel and device tree in the Image Store (the
`raspi*` machines select `qemu-system-aarch64` for you), and:

> *"Boot a Raspberry Pi 3 and show me the console."*

```json
{
  "machine": "raspi3b",
  "accel": "tcg",
  "kernel": "kernel8.img",
  "dtb": "bcm2710-rpi-3-b.dtb",
  "appendCmdline": "console=tty1 root=/dev/mmcblk0p2 rootwait rw",
  "disks": [{ "image": "raspios.img", "interface": "sd", "format": "raw" }],
  "network": { "model": "usb-net" },
  "display": "vnc"
}
```

`console=tty1` puts the console on the framebuffer, so the noVNC Viewer shows the Pi booting
— logos and all. No `cpu`/`vcpus`/`memoryMb`: the board's hardware is fixed. The Pi has no
PCI bus, so the default NIC can't attach — use `"network": { "model": "usb-net" }` for its USB
NIC, or `"network": { "mode": "none" }` for no networking at all. (On a Pi 3, merge the
`disable-bt` device-tree overlay into the dtb first, or the console stays glued to the
Bluetooth-shared UART instead of the screen.)

## Choosing an implementation

The two are interchangeable — same tools, same specs, same behavior, continuously checked
against each other. Pick by ecosystem:

| | TypeScript | Rust |
| --- | --- | --- |
| Built on | Node + `mcp-framework` | `rmcp` + tokio |
| Ships as | an npm package / `node dist/index.js` | a single self-contained binary |
| Get it running | [Run it →](typescript/README.md#run-it) | [Run it →](rust/README.md#run-it) |
| In Docker | [Docker →](typescript/README.md#docker) | [Docker →](rust/README.md#docker) |

Everything deployment- and usage-specific lives in those two READMEs:

- **TypeScript** — [Run it](typescript/README.md#run-it) ·
  [Transports & auth](typescript/README.md#transports) ·
  [Docker](typescript/README.md#docker) ·
  [Browser viewer](typescript/README.md#browser-viewer) ·
  [Configuration](typescript/README.md#configuration) ·
  [Developing](typescript/README.md#developing)
- **Rust** — [Run it](rust/README.md#run-it) ·
  [Transports & auth](rust/README.md#transports) ·
  [Docker](rust/README.md#docker) ·
  [KVM acceleration](rust/README.md#faster-vms-with-kvm) ·
  [Browser viewer](rust/README.md#browser-viewer) ·
  [Cross-compilation](rust/README.md#building-for-other-platforms) ·
  [Configuration](rust/README.md#configuration) ·
  [Developing](rust/README.md#developing)

## Configuration

Both implementations are configured entirely through `QMP_MCP_*` environment variables —
**the same names and defaults for each**. The fully-commented reference is
[`.env.example`](.env.example), and the command-policy file format is
[`policy.example.yaml`](policy.example.yaml). The ones you'll reach for:

| Variable | Default | What it does |
| --- | --- | --- |
| `QMP_MCP_TRANSPORT` | `stdio` | `stdio`, `http`, or `both` |
| `QMP_MCP_API_KEYS` | _(unset)_ | API keys for the HTTP transport (required unless insecure) |
| `QMP_MCP_QEMU_BINARY` | _(derived from `machine`)_ | override the emulator for every Instance; unset derives it (q35→x86_64, virt/raspi*→aarch64, ADR-0013) |
| `QMP_MCP_IMAGE_DIR` / `QMP_MCP_ISO_DIR` | XDG paths | the Image Store / ISO Store folders |
| `QMP_MCP_VIEWER_PASSWORD` | _(unset)_ | enables the browser Viewer |
| `QMP_MCP_ALLOW_RAW_ARGS` | `false` | allow a spec's `extraArgs` (the escape hatch) |

…plus caps on disk/memory/vCPUs, the host-forward port range, the Command Policy allow/deny
lists and policy file, and the Event Buffer size. See [`.env.example`](.env.example) for
the full list, or each variant's Configuration section in context
([TypeScript](typescript/README.md#configuration) · [Rust](rust/README.md#configuration)).

## For developers

### Layout

```
qmp-mcp/
├── typescript/          the Node / mcp-framework implementation
├── rust/                the Rust / rmcp implementation
├── testdata/            shared golden fixtures both implementations assert
├── docs/                design notes and rationale
├── CONTEXT.md           the domain glossary — the shared vocabulary
├── .env.example         every QMP_MCP_* variable, commented
└── policy.example.yaml  the command-policy file format
```

The two implementations are independent codebases that share three things at the root: the
**domain model** ([`CONTEXT.md`](CONTEXT.md) — read it first), the **golden fixtures**
([`testdata/`](testdata/)), and the **config surface** ([`.env.example`](.env.example)).

### How the two stay identical

Parity here isn't a promise, it's a test. [`testdata/`](testdata/) holds language-neutral
golden fixtures that pin the exact QEMU command line each Hardware Spec must produce and the
exact verdict the Command Policy must return — and **both** implementations are tested
against that same corpus. Change how a spec becomes a command line, or what the policy
allows, and you update the shared fixture; the TypeScript suite *and* the Rust suite have to
agree, or the build fails. Teach one implementation a new trick and you add the fixture the
other has to satisfy.

Working on a variant is self-contained in its folder —
[developing TypeScript](typescript/README.md#developing) ·
[developing Rust](rust/README.md#developing). The [`docs/`](docs/) folder holds the
longer-form rationale behind the trickier decisions.

## License

[MIT](LICENSE).
