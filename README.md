# qmp-mcp

Give an AI agent its own virtual machine — one it can build, boot, drive, and tear
down on its own, without ever letting it near your host.

**qmp-mcp** is a [Model Context Protocol](https://modelcontextprotocol.io) server that
hands an AI assistant the controls of a single [QEMU](https://www.qemu.org) virtual
machine. The agent describes the hardware it wants — CPU, memory, disks, a boot ISO,
networking — and the server builds and runs it, then exposes tools to pause it, reset
it, watch its screen, send commands over QEMU's control channel, and clean it up when
it's done.

The trick that makes this safe to hand to an autonomous agent: it never gets to pass
raw arguments to QEMU or touch arbitrary files on your machine. It fills in a
structured *hardware spec* that the server validates and turns into a locked-down QEMU
command line. Everything the agent can reach — disk images, boot ISOs, the commands it
can send the running VM — goes through allowlists you control.

## Why you might want this

- **Let an agent do real systems work.** Spin up a throwaway Linux box, install
  something, test it, tear it down — driven entirely by an assistant, on a real kernel
  with real hardware emulation, not a sandbox that only pretends.
- **Keep it contained.** The VM is the blast radius. The agent can't run arbitrary
  QEMU flags, can't read or write files outside the folders you designate, and can't
  open network ports you didn't allow. The server runs unprivileged and won't expose
  anything over the network without authentication.
- **Watch it happen.** Turn on the optional browser viewer and you get a live noVNC
  window into the guest's screen — handy for booting an installer, or just seeing what
  the agent is up to.

## What the agent can do

Once it's connected, an assistant has tools to:

- **Build & run a VM** from a validated hardware spec (machine type, CPU, memory,
  vCPUs, disks, a boot ISO, a display) — and tear it back down.
- **Drive its lifecycle** — pause and resume the CPUs, hard-reset it, ask for a
  graceful shutdown.
- **Inspect it** — the current run state, block devices, per-CPU info, and a PNG
  screenshot of the display.
- **Send control commands** to the live VM (`qmp_execute`), gated by a policy so only
  safe ones get through.
- **React to events** — read the VM's recent QEMU events, or block until a specific
  one arrives (a shutdown, a reset, …).
- **Manage media** — create disk images, and list the disks and boot ISOs it's
  allowed to use.

## Two implementations, same behavior

This repo ships the server twice:

- **[`typescript/`](typescript/)** — the original, on Node and the `mcp-framework` SDK.
- **[`rust/`](rust/)** — a second implementation on the official Rust MCP SDK (`rmcp`)
  and tokio, distributable as a single self-contained binary.

They are behaviorally identical: same tools, same hardware-spec validation, same
security rules, same environment variables. And they're kept honest against each
other — a shared set of golden fixtures in [`testdata/`](testdata/) pins the exact QEMU
command line each hardware spec must produce and the exact verdict the command policy
must return, and **both** servers are tested against that same corpus. Drift on either
side fails the build.

Pick whichever fits your stack. Not sure? Start with `typescript/` if you live in Node;
reach for `rust/` if you'd rather ship one binary.

## Quick start (using it)

You need **QEMU installed** wherever the server runs — it shells out to `qemu-system-*`
and `qemu-img`. (The Docker images bundle it for you.)

Nothing is published to a public registry yet, so the paths that work today start from
a checkout of this repo.

### Run it in Docker

```bash
git clone <this-repo> qmp-mcp && cd qmp-mcp

# TypeScript image:
docker build -f typescript/Dockerfile -t qmp-mcp typescript
# …or the Rust image:
docker build -f rust/Dockerfile -t qmp-mcp:rust rust

docker run --rm -p 8080:8080 \
  -e QMP_MCP_API_KEYS=pick-a-strong-key \
  -v qmp-images:/var/lib/qmp-mcp/images \
  -v qmp-isos:/var/lib/qmp-mcp/isos \
  qmp-mcp            # or qmp-mcp:rust
```

The HTTP transport **won't start without authentication** — you either give it an API
key (sent in the `X-API-Key` header) or explicitly opt into insecure mode for local
tinkering. A server that can build and run VMs has no business being reachable
unauthenticated.

Faster VMs are one flag away: add `--device /dev/kvm --group-add "$(getent group kvm |
cut -d: -f3)"` for hardware acceleration. Without it you get TCG software emulation,
which works anywhere with zero privileges.

### Or point a stdio client straight at it

Most MCP clients launch a server over stdio. From a checkout:

```jsonc
{
  "mcpServers": {
    "qmp": {
      "command": "node",                     // TypeScript: after `cd typescript && npm ci && npm run build`
      "args": ["typescript/dist/index.js"],
      "env": {
        "QMP_MCP_IMAGE_DIR": "/srv/qmp/images",
        "QMP_MCP_ISO_DIR":   "/srv/qmp/isos"
      }
    }
  }
}
```

Then ask your assistant something like *"create a VM with 2 GB of RAM and boot the
Alpine ISO in my ISO folder"* and watch it call `create_instance`.

Per-implementation install and run details (npm/`npx`, `cargo install`, transports,
the browser viewer) live in **[`typescript/README.md`](typescript/README.md)** and
**[`rust/README.md`](rust/README.md)**.

### Watching the screen

Set `QMP_MCP_VIEWER_PASSWORD`, ask for a VM with a `vnc` display, and open
`http://<host>:6080/` in a browser (publish port `6080` too if you're in Docker). You
get an interactive noVNC session into the guest. The raw VNC port never leaves
loopback, and the viewer is password-gated.

## How it keeps you safe

Handing VM controls to an autonomous agent only works if the controls *are* the
boundary. The choices that make that true:

- **No raw QEMU.** The agent fills a structured hardware spec; the server generates the
  command line. Fields are range- and character-checked, and values that could smuggle
  extra options (a comma inside a `-drive`, say) are escaped or rejected. There's an
  `extraArgs` escape hatch for raw flags — off unless you turn it on.
- **Files stay in their lane.** Guest disk images live in one folder you designate
  (read-write); boot ISOs in another (read-only). The agent references them by name,
  and a name that tries to climb out — `../`, a symlink, an absolute path — is refused.
- **A command allowlist.** The generic `qmp_execute` runs arbitrary QEMU Machine
  Protocol commands, but only ones on a default-safe allowlist. A hard denylist can
  never be re-enabled; you can tighten or widen the rest with an env var or a policy
  file.
- **Sandboxed networking.** Guests get user-mode networking by default; port-forwards
  are limited to a non-privileged range and bound to loopback.
- **Fail-closed and unprivileged.** The HTTP transport refuses to start without auth.
  The server runs as a non-root user and never needs `--privileged` — hardware
  acceleration is an opt-in device, not a requirement.

## Configuration

Everything is configured through `QMP_MCP_*` environment variables — the **same names
and defaults for both implementations**. The fully-commented reference is in
**[`.env.example`](.env.example)**, and the command-policy file format in
**[`policy.example.yaml`](policy.example.yaml)**. The ones you'll reach for most:

| Variable | Default | What it does |
| --- | --- | --- |
| `QMP_MCP_TRANSPORT` | `stdio` | `stdio`, `http`, or `both` |
| `QMP_MCP_API_KEYS` | _(unset)_ | comma-separated API keys for the HTTP transport (required unless insecure) |
| `QMP_MCP_QEMU_BINARY` | `qemu-system-x86_64` | which emulator to launch — set `qemu-system-aarch64` for ARM guests, etc. |
| `QMP_MCP_IMAGE_DIR` / `QMP_MCP_ISO_DIR` | XDG paths | the read-write disk folder / read-only ISO folder |
| `QMP_MCP_VIEWER_PASSWORD` | _(unset)_ | enables the noVNC viewer (required to request a `vnc` display) |
| `QMP_MCP_ALLOW_RAW_ARGS` | `false` | let a spec pass raw QEMU flags (the escape hatch) |

…plus caps on disk/memory/vCPUs, the port-forward range, the command-policy allow/deny
lists, and the event-buffer size. See `.env.example` for the whole list.

## For developers

### Layout

```
qmp-mcp/
├── typescript/          the Node / mcp-framework implementation
├── rust/                the Rust / rmcp implementation
├── testdata/            shared golden fixtures both implementations test against
├── docs/                design notes and rationale
├── CONTEXT.md           the domain glossary — the shared vocabulary
├── .env.example         every QMP_MCP_* variable, commented
└── policy.example.yaml  the command-policy file format
```

The two implementations are independent codebases that share three things at the repo
root: the **domain model** ([`CONTEXT.md`](CONTEXT.md) — read it first; words like
*Instance*, *Hardware Spec*, *Command Policy*, *Image Store*, and *Viewer* mean
specific things), the **golden fixtures** ([`testdata/`](testdata/)), and the **config
surface** ([`.env.example`](.env.example)).

### Working on a variant

Each implementation is self-contained in its folder, with its own README, build, and
tests:

- **TypeScript** → [`typescript/README.md`](typescript/README.md). In short:
  `cd typescript && npm ci && npm test`.
- **Rust** → [`rust/README.md`](rust/README.md). In short: `cd rust && cargo test`.

This repo runs its toolchains in Docker so they never clutter your host — each variant's
README has the specifics.

### Keeping the two in sync

The whole point of `testdata/` is that parity isn't a promise, it's a test. Change how
a hardware spec becomes a command line, or what the command policy allows, and you
update the shared fixture — which is asserted by the TypeScript suite *and* the Rust
suite. So when you teach one implementation a new trick, add the matching fixture and
the other has to keep up.

The [`docs/`](docs/) folder has the longer-form rationale behind the trickier design
decisions, if you want the "why."

## License

[MIT](LICENSE).
