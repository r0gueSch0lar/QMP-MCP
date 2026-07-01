# qmp-mcp

A secure [Model Context Protocol](https://modelcontextprotocol.io) server that lets an
AI agent build, launch, drive, and tear down a single QEMU virtual machine (the
**Instance**) through QEMU's [QMP](https://www.qemu.org/docs/master/interop/qmp-spec.html)
API. The agent never supplies raw QEMU arguments: it fills a structured, validated
**Hardware Spec** and the server generates the `qemu-system-*` argv from it.

Built on [`mcp-framework`](https://mcp-framework.com). See [`CONTEXT.md`](./CONTEXT.md)
for the domain glossary (Instance, Guest, QMP Session, Hardware Spec, Command Policy,
Image/ISO Store, Event Buffer) and [`docs/adr/`](./docs/adr) for the architectural
decisions referenced throughout this file.

The server runs equally as an ordinary process via `npx`/`node` on a host with QEMU
installed (stdio) **and** as a container exposing the HTTP transport — bare metal is a
co-equal target (ADR-0007). It runs **non-root** in both modes and never needs
`--privileged` (ADR-0008).

## Run on bare metal (stdio)

Requires Node.js >= 20 and `qemu-system-x86_64` + `qemu-img` on `PATH`.

```bash
npx -y qmp-mcp
```

This speaks the stdio transport — the shape MCP clients launch directly. Example
client config:

```json
{
  "mcpServers": {
    "qmp": {
      "command": "npx",
      "args": ["-y", "qmp-mcp"],
      "env": {
        "QMP_MCP_IMAGE_DIR": "/srv/qmp/images",
        "QMP_MCP_ISO_DIR": "/srv/qmp/isos"
      }
    }
  }
}
```

## Run with Docker (HTTP)

The image (ADR-0007) ships a slim Debian+Node runtime with `qemu-system-x86` +
`qemu-utils`, runs as a non-root user, and defaults to the **HTTP** transport bound to
`0.0.0.0` so a published port is reachable.

```bash
docker build -t qmp-mcp .

docker run --rm -p 8080:8080 \
  -e QMP_MCP_API_KEYS=replace-with-a-strong-key \
  qmp-mcp
```

The HTTP transport **fails closed** (ADR-0005): it refuses to start unless you provide
auth — `-e QMP_MCP_API_KEYS=...` (the `X-API-Key` header) or JWT
(`-e QMP_MCP_AUTH=jwt -e QMP_MCP_JWT_SECRET=...`). For throwaway local use you can opt
out with `-e QMP_MCP_ALLOW_INSECURE=true`, but never expose that on an untrusted
network.

Persist the Stores by mounting the container's Store directories:

```bash
docker run --rm -p 8080:8080 \
  -e QMP_MCP_API_KEYS=replace-with-a-strong-key \
  -v qmp-images:/var/lib/qmp-mcp/images \
  -v qmp-isos:/var/lib/qmp-mcp/isos \
  qmp-mcp
```

### Optional KVM acceleration

By default the Instance uses TCG software emulation (no privileges required). To use
hardware **KVM**, pass the device and join the `kvm` group — and nothing more. The
container is **never** run `--privileged` (ADR-0008):

```bash
docker run --rm -p 8080:8080 \
  -e QMP_MCP_API_KEYS=replace-with-a-strong-key \
  --device /dev/kvm \
  --group-add "$(getent group kvm | cut -d: -f3)" \
  qmp-mcp
```

`accel: 'auto'` (the Hardware Spec default) probes `/dev/kvm` and uses KVM when it is
accessible, otherwise falls back to TCG, reporting which it chose. `accel: 'kvm'`
hard-fails with an actionable message when `/dev/kvm` is unavailable.

## Browser Viewer (noVNC)

The **Display** is the Guest's graphical output; the **Viewer** is an optional
in-process [noVNC](https://novnc.com) bridge that lets you watch and control it in a
browser (ADR-0010). It is off by default and interactive (keyboard + mouse), not
view-only.

To use it, set a Viewer password and request a `vnc` Display when you create the
Instance:

- Set `QMP_MCP_VIEWER_PASSWORD` to a strong password. Without it, requesting a `vnc`
  Display **rejects** `create_instance` with an actionable error — the Viewer is
  fail-closed and refuses to serve without its own password.
- Call `create_instance` with `display: "vnc"` in the Hardware Spec. The server
  attaches a **loopback-only** VNC server to the Guest, arms it with an internal
  password over QMP (never in the process list), and starts the Viewer for the
  lifetime of that Instance. `destroy_instance` stops it.
- Open `http://<host>:6080/` in a browser (the Viewer runs on its own HTTP server,
  independent of the MCP transport — it works even under `stdio`). Authenticate with
  `QMP_MCP_VIEWER_PASSWORD` (HTTP Basic; any username). The VNC handshake is then
  automatic — `QMP_MCP_VIEWER_PASSWORD` is the only secret you type.

With Docker, publish the Viewer port alongside the MCP port and pass the password:

```bash
docker run --rm -p 8080:8080 -p 6080:6080 \
  -e QMP_MCP_API_KEYS=replace-with-a-strong-key \
  -e QMP_MCP_VIEWER_PASSWORD=replace-with-a-strong-viewer-password \
  qmp-mcp
```

The raw VNC port (5900) is **loopback-only and never published** — the Viewer is its
sole client, and its bytes are proxied only to that server-controlled loopback port,
never to any client-supplied target.

**Cleartext exposure.** The Viewer serves plain HTTP: the Basic password and the
interactive VNC session travel **unencrypted**. On loopback (`QMP_MCP_VIEWER_HOST` left
at `127.0.0.1`) that is fine. When it binds a non-loopback address (the container image
sets `0.0.0.0` so a published port is reachable) it logs a startup **WARNING** and you
should put it **behind a TLS-terminating reverse proxy** on any untrusted network. The
Viewer also refuses to be framed (`X-Frame-Options: DENY` + a `frame-ancestors 'none'`
CSP), rejects cross-origin websocket upgrades, and caps concurrent connections.

## Tools

| Tool | What it does |
| --- | --- |
| `get_instance` | Return the current Instance and its lifecycle state (`NONE` when none runs). |
| `create_instance` | Build and launch the Instance from a Hardware Spec, negotiate its QMP session, reach `RUNNING`, and report the chosen accelerator. |
| `destroy_instance` | Terminate the Instance and tear down its QMP session, returning to `NONE`. |
| `pause_instance` | Pause the Guest CPUs (QMP `stop`) → `PAUSED`. |
| `resume_instance` | Resume the Guest CPUs (QMP `cont`) → `RUNNING`. |
| `reset_instance` | Hard-reset the Instance (QMP `system_reset`). |
| `powerdown_instance` | Request a graceful ACPI shutdown (QMP `system_powerdown`). |
| `get_status` | Live run state of the Guest CPUs (QMP `query-status`). |
| `list_block_devices` | The Instance's block devices and backing media (QMP `query-block`). |
| `query_cpus` | Per-vCPU information (QMP `query-cpus-fast`). |
| `screendump` | Capture the Instance's display as a PNG, returned inline. |
| `get_events` | Recently buffered QMP async events, cursor-paged, without blocking. |
| `wait_for_event` | Long-poll until a matching QMP async event arrives (or time out). |
| `qmp_execute` | Run a generic QMP command, gated by the Command Policy (ADR-0003). |
| `create_image` | Create a blank disk image (name, size, format) inside the Image Store. |
| `list_images` | List guest disk images available in the Image Store, by name. |
| `list_isos` | List install/boot ISO media available in the read-only ISO Store, by name. |

## Configuration

Configured entirely through `QMP_MCP_*` environment variables. Invalid values **fail
closed** at startup with a message naming the variable and its allowed values. The
exhaustive, commented reference (with defaults) is [`.env.example`](./.env.example) —
the most-used variables:

| Variable | Default | Purpose |
| --- | --- | --- |
| `QMP_MCP_TRANSPORT` | `stdio` | Transport: `stdio` \| `http` \| `both`. (The image overrides to `http`.) |
| `QMP_MCP_LOG_LEVEL` | `info` | Logger verbosity: `debug` \| `info` \| `warning` \| `error`. |
| `QMP_MCP_HTTP_HOST` | `127.0.0.1` | HTTP bind address. (The image overrides to `0.0.0.0`.) |
| `QMP_MCP_HTTP_PORT` | `8080` | HTTP listen port. |
| `QMP_MCP_HTTP_ENDPOINT` | `/mcp` | HTTP MCP endpoint path. |
| `QMP_MCP_HTTP_ALLOWED_ORIGINS` | loopback origins | Comma-separated browser origins for the DNS-rebinding/CORS guard. |
| `QMP_MCP_AUTH` | `apikey` | HTTP auth provider: `apikey` \| `jwt`. |
| `QMP_MCP_API_KEYS` | _(empty)_ | Comma-separated API keys (`X-API-Key`) for `apikey` auth. |
| `QMP_MCP_JWT_SECRET` | _(unset)_ | HS256 signing secret for `jwt` auth. |
| `QMP_MCP_ALLOW_INSECURE` | `false` | Run HTTP unauthenticated (local dev only). |
| `QMP_MCP_IMAGE_DIR` | XDG/HOME path | Read-write Image Store directory (ADR-0006). |
| `QMP_MCP_ISO_DIR` | XDG/HOME path | Read-only ISO Store directory (ADR-0006). |
| `QMP_MCP_MAX_DISK_GB` | `64` | Hard cap on a created image's virtual size (GiB). |
| `QMP_MCP_MAX_MEMORY_MB` | `4096` | Hard cap on a spec's `memoryMb`. |
| `QMP_MCP_MAX_VCPUS` | `2` | Hard cap on a spec's `vcpus`. |
| `QMP_MCP_HOSTFWD_PORT_RANGE` | `1024-65535` | Allowed host-port range for user-mode port-forwards (ADR-0009). |
| `QMP_MCP_ALLOW_HOST_NET` | `false` | Permit host-level (`tap`/`bridge`) networking (ADR-0009). |
| `QMP_MCP_ALLOW` / `QMP_MCP_DENY` | _(empty)_ | Add/remove QMP commands on the Command Policy allowlist (ADR-0003). |
| `QMP_MCP_POLICY_FILE` | _(unset)_ | YAML policy file layered onto the allowlist. |
| `QMP_MCP_EVENT_BUFFER_SIZE` | `256` | Capacity of the Event Buffer of recent QMP async events. |
| `QMP_MCP_ALLOW_RAW_ARGS` | `false` | Allow a spec's `extraArgs` (raw QEMU args) — gated escape hatch (ADR-0002). |
| `QMP_MCP_VIEWER_PASSWORD` | _(unset)_ | Password gating the noVNC Viewer; required to request `display: vnc` (ADR-0010). |
| `QMP_MCP_VIEWER_HOST` | `127.0.0.1` | Viewer HTTP bind address. (The image overrides to `0.0.0.0`.) |
| `QMP_MCP_VIEWER_PORT` | `6080` | Viewer HTTP listen port. |

## Security

- **Fail-closed HTTP auth (ADR-0005).** The HTTP transport can build and control VMs,
  so it refuses to start without configured auth (API key or JWT) unless you explicitly
  opt into insecure mode. A DNS-rebinding/CORS origin allowlist guards browser callers.
- **No raw argv; Hardware Spec only (ADR-0002).** The agent fills a validated Hardware
  Spec — closed enums and allowlisted names, no free-text that could inject QEMU
  options. The raw `extraArgs` escape hatch is **disabled by default** and refused
  unless `QMP_MCP_ALLOW_RAW_ARGS=true` (trusted single-tenant hosts only).
- **Allowlisted Stores (ADR-0006).** Disks and ISOs are referenced by name within a
  single read-write Image Store and a separate read-only ISO Store; absolute paths,
  `..` traversal, and symlink escapes are rejected, so storage selection never reaches
  the host filesystem.
- **Command Policy (ADR-0003).** The generic `qmp_execute` tool is gated by a
  default-safe allowlist plus an immutable hard denylist (`human-monitor-command`,
  `migrate`, `dump-guest-memory`, …) that no override can re-enable.
- **Resource caps (issue #9) & networking (ADR-0009).** Per-deployment caps bound guest
  memory and vCPUs; user-mode (SLiRP) networking is the default with inbound only via
  bounded, loopback-pinned port-forwards. Host networking needs an explicit opt-in.
- **Non-root, KVM optional, never `--privileged` (ADR-0008).** The server runs as a
  non-root user; the only device it may be granted is `/dev/kvm`, and only as an opt-in
  performance upgrade — TCG works with zero privileges.
- **Fail-closed Viewer (ADR-0010).** The optional noVNC Viewer exists only while a
  `display: vnc` Instance runs and refuses both the page and the websocket unless the
  request authenticates with `QMP_MCP_VIEWER_PASSWORD`. The Guest's VNC server binds
  loopback only (never published) behind a second, server-set VNC password; the
  websocket proxy always dials that server-controlled loopback port, never a
  client-supplied target, and static serving is confined to the noVNC assets.

## Development

```bash
npm install
npm run build      # tsc -> dist/
npm run typecheck  # tsc --noEmit
npm run test       # vitest
npm run lint       # biome
npm run format     # biome --write
```

Requires Node.js >= 20.
