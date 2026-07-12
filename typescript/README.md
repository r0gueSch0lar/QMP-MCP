# qmp-mcp (TypeScript)

The Node implementation of **qmp-mcp** — an MCP server that lets an AI agent build,
boot, drive, and tear down a single QEMU virtual machine. For the big picture (what it
is, why it's safe to hand an agent, how the two implementations relate), start with the
[project README](../README.md). This page is about running and developing the
TypeScript variant.

## Requirements

- **Node 22+** (with **pnpm** as the package manager — pinned via package.json `packageManager`;
  run `corepack enable` once and it's activated automatically)
- **QEMU** on your `PATH` at runtime — the emulator is picked from the spec's `machine`
  (`qemu-system-x86_64`, `qemu-system-aarch64` for ARM/raspi, …; override with
  `QMP_MCP_QEMU_BINARY`), plus `qemu-img`.

## Run it

From a checkout of this repo:

```bash
cd typescript
pnpm install           # or: corepack enable && pnpm install
pnpm build             # compiles to dist/
node dist/index.js     # an MCP server over stdio
```

That's the shape most MCP clients launch directly. Point your client at it:

```json
{
  "mcpServers": {
    "qmp": {
      "command": "node",
      "args": ["/path/to/qmp-mcp/typescript/dist/index.js"],
      "env": {
        "QMP_MCP_IMAGE_DIR": "/srv/qmp/images",
        "QMP_MCP_ISO_DIR":   "/srv/qmp/isos"
      }
    }
  }
}
```

Once the package is published to npm, `npx -y qmp-mcp` will be the no-checkout shortcut
for exactly this.

### Transports

`QMP_MCP_TRANSPORT` decides how it talks: `stdio` (the default — no network, no auth
needed), `http`, or `both`.

```bash
# HTTP — won't start without auth
QMP_MCP_TRANSPORT=http QMP_MCP_API_KEYS=pick-a-strong-key node dist/index.js
```

The HTTP transport is **fail-closed**: no API key (or JWT secret) means it refuses to
start — unless you set `QMP_MCP_ALLOW_INSECURE=true` for local-only use. Keys travel in
the `X-API-Key` header.

## Docker

The image bundles QEMU and runs the server as a non-root user, defaulting to the HTTP
transport bound to all interfaces:

```bash
# from the repo root:
docker build -f typescript/Dockerfile -t qmp-mcp typescript
docker run --rm -p 8080:8080 -e QMP_MCP_API_KEYS=pick-a-strong-key qmp-mcp
```

Persist the disk and ISO folders with volumes (`-v qmp-images:/var/lib/qmp-mcp/images
-v qmp-isos:/var/lib/qmp-mcp/isos`), and add `--device /dev/kvm --group-add "$(getent
group kvm | cut -d: -f3)"` for hardware acceleration. Without KVM it falls back to TCG
software emulation — slower, but it needs no privileges. The container is never run
`--privileged`.

## Browser viewer

Set `QMP_MCP_VIEWER_PASSWORD`, ask for a VM with `display: "vnc"`, and open
`http://<host>:6080/` — an interactive noVNC session into the guest's screen (publish
`6080` in Docker). It's password-gated (HTTP Basic), the raw VNC port stays on
loopback, and it's off entirely until you set the password.

## The tools

| Tool | What it does |
| --- | --- |
| `create_instance` / `destroy_instance` | build & launch a VM from a hardware spec / tear it down |
| `get_instance` / `get_status` | the current instance + lifecycle state / the live guest run state |
| `get_share` | report the host↔guest folder-sharing config + the exact 9p mount command |
| `get_serial` / `read_serial` / `write_serial` | report the Serial Port config + console device / drain the Guest's serial output / type input into the console (gated by `QMP_MCP_ALLOW_SERIAL_WRITE`) |
| `pause_instance` / `resume_instance` | freeze / unfreeze the guest CPUs |
| `reset_instance` / `powerdown_instance` | hard reset / graceful ACPI shutdown |
| `list_block_devices` / `query_cpus` | the VM's disks / per-CPU info |
| `screendump` | a PNG screenshot of the display |
| `get_events` / `wait_for_event` | recent QEMU events / block until a named one arrives |
| `qmp_execute` | a raw QMP command, gated by the command policy |
| `create_image` / `list_images` / `list_isos` | make a disk image / list disks / list boot ISOs |

## Configuration

Everything is `QMP_MCP_*` environment variables — the same names and defaults as the
Rust variant. The full, commented list is in [`../.env.example`](../.env.example) (and
the command-policy file format in [`../policy.example.yaml`](../policy.example.yaml)).
The ones you'll reach for: `QMP_MCP_TRANSPORT`, `QMP_MCP_API_KEYS`, `QMP_MCP_QEMU_BINARY`
(usually unset — the emulator is derived from `machine`; ADR-0013), `QMP_MCP_IMAGE_DIR` / `QMP_MCP_ISO_DIR`,
`QMP_MCP_VIEWER_PASSWORD`, plus caps on disk/memory/vCPUs and the command-policy
allow/deny lists.

## Developing

Toolchains run in Docker so they never touch your host (that dev container setup is a
local, uncommitted convenience). The scripts:

```bash
cd typescript
pnpm install          # pnpm is pinned via package.json "packageManager"; corepack activates it
pnpm lint             # biome
pnpm typecheck        # tsc --noEmit
pnpm test             # vitest
pnpm build            # tsc -> dist/
```

The test suite includes the cross-implementation parity tests in `test/`, which assert
the shared golden fixtures in [`../testdata/`](../testdata/) against this server's
spec→argv generator and command policy — the *same* fixtures the Rust variant asserts.
Change either the generator or the policy and you update the fixture, and both suites
have to agree. New behavior on one side means a new fixture the other has to satisfy.

## License

[MIT](../LICENSE).
