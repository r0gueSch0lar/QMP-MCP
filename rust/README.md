# qmp-mcp (Rust)

The Rust implementation of **qmp-mcp** — an MCP server that lets an AI agent build,
boot, drive, and tear down a single QEMU virtual machine. For the big picture (what it
is, why it's safe to hand an agent, how the two implementations relate), start with the
[project README](../README.md). This page is about running and developing the Rust
variant.

It's built on the official [Rust MCP SDK](https://docs.rs/rmcp) (`rmcp`) and
[tokio](https://tokio.rs), and it's behavior-for-behavior identical to the
[TypeScript variant](../typescript) — same tools, same hardware-spec validation, same
command policy, same environment variables. The crate and binary are both named
`qmp-mcp`, and it ships as a single self-contained binary (the browser-viewer assets are
baked right in), so it runs just as happily as a plain process on a host with QEMU as it
does in a container.

## Requirements

- **Rust 1.96+** (edition 2021) to build.
- **QEMU** on your `PATH` at runtime — the emulator is picked from the spec's `machine`
  (`qemu-system-x86_64`, `qemu-system-aarch64` for ARM/raspi, …; override with
  `QMP_MCP_QEMU_BINARY`), plus `qemu-img`.
- Builds and runs on **Linux** (x86-64, ARM64) and **macOS** (Intel & Apple Silicon) —
  see [Building for other platforms](#building-for-other-platforms).

## Run it

From the crate directory:

```bash
cd rust
cargo run --release        # an MCP server over stdio (the default transport)
```

Or install the `qmp-mcp` binary onto your `PATH` — the Rust equivalent of `npx`, once
compiled it's just a command:

```bash
cargo install --locked --path rust     # from the repo root
qmp-mcp                                 # then just run it
```

`--locked` builds against the committed `Cargo.lock` for a reproducible result. Point an
MCP client straight at the binary:

```json
{
  "mcpServers": {
    "qmp": {
      "command": "qmp-mcp",
      "env": {
        "QMP_MCP_IMAGE_DIR": "/srv/qmp/images",
        "QMP_MCP_ISO_DIR":   "/srv/qmp/isos"
      }
    }
  }
}
```

### Transports

`QMP_MCP_TRANSPORT` decides how it talks: `stdio` (the default — no network, no auth
needed), `http`, or `both`.

```bash
# HTTP — won't start without auth
QMP_MCP_TRANSPORT=http QMP_MCP_API_KEYS=pick-a-strong-key qmp-mcp
```

The HTTP transport is **fail-closed**: no credentials, no start — unless you set
`QMP_MCP_ALLOW_INSECURE=true` for local-only use. Two ways to authenticate:

- **API key** (the default) — send it in the `X-API-Key` header.
- **JWT** — set `QMP_MCP_AUTH=jwt` and a `QMP_MCP_JWT_SECRET`, then send
  `Authorization: Bearer <token>`. Tokens are verified against the secret and pinned to
  HS256 (anything presenting `alg: none` or another algorithm is rejected).

## Docker

The image is a cargo-chef multi-stage build onto a slim `debian:bookworm-slim` runtime
with QEMU, running as a non-root user and defaulting to the HTTP transport bound to all
interfaces. Because the viewer assets are embedded in the binary, the runtime image
copies exactly one file — the compiled server. It's tagged distinctly from the
TypeScript image so the two never collide:

```bash
# from the repo root:
docker build -f rust/Dockerfile -t qmp-mcp:rust rust
docker run --rm -p 8080:8080 -e QMP_MCP_API_KEYS=pick-a-strong-key qmp-mcp:rust
```

Persist the disk and ISO folders with volumes (`-v qmp-images:/var/lib/qmp-mcp/images
-v qmp-isos:/var/lib/qmp-mcp/isos`).

### Faster VMs with KVM

By default a VM uses TCG software emulation, which needs no privileges. For hardware
**KVM**, pass the device and join the group — and nothing more; the container is never
run `--privileged`:

```bash
docker run --rm -p 8080:8080 \
  -e QMP_MCP_API_KEYS=pick-a-strong-key \
  --device /dev/kvm \
  --group-add "$(getent group kvm | cut -d: -f3)" \
  qmp-mcp:rust
```

With `accel: "auto"` (the hardware-spec default) the server uses KVM when `/dev/kvm` is
reachable and quietly falls back to TCG otherwise, reporting which it chose. Ask for
`accel: "kvm"` explicitly and it fails with a clear message if KVM isn't available.

## Browser viewer

Turn on the optional [noVNC](https://novnc.com) viewer and you get an interactive
window into the guest's screen (keyboard + mouse), in your browser:

- Set `QMP_MCP_VIEWER_PASSWORD` to a strong password. It's off until you do, and
  requesting a `vnc` display without it is refused up front.
- Create a VM with `display: "vnc"`. The server attaches a loopback-only VNC server to
  the guest, arms it with an internal password over QEMU's control channel (never on the
  process command line), and runs the viewer for that VM's lifetime.
- Open `http://<host>:6080/` and sign in with the viewer password (HTTP Basic, any
  username). The viewer runs on its own HTTP server, independent of the MCP transport, so
  it works even under `stdio`. In Docker, publish `6080` alongside `8080`.

The raw VNC port never leaves loopback. The viewer serves plain HTTP, so on a
non-loopback bind it warns you at startup — put a TLS-terminating proxy in front of it on
any untrusted network. It also refuses to be framed, rejects cross-origin websocket
upgrades, and caps concurrent connections.

## Building for other platforms

The server is portable Rust and builds for Linux and macOS. The default target is x86-64
Linux; pass `--target <triple>` for the rest, running cargo from the crate directory so
the per-target linker settings in [`.cargo/config.toml`](./.cargo/config.toml) apply. Add
a target's standard library once with `rustup target add <triple>`. Whatever you build,
the host it runs on needs `qemu-system-*` + `qemu-img` at runtime.

| Target | Platform | Build on |
| --- | --- | --- |
| `x86_64-unknown-linux-gnu` | Linux x86-64 (default) | Linux — `cargo build --release` |
| `aarch64-unknown-linux-gnu` | Linux ARM64 | Linux (cross — see below) |
| `aarch64-apple-darwin` | macOS Apple Silicon | macOS |
| `x86_64-apple-darwin` | macOS Intel | macOS |

Release binaries are stripped for size.

**macOS** binaries must be built on a Mac (the Apple SDK and linker are required;
cross-building them from Linux needs osxcross, which we don't cover):

```bash
rustup target add aarch64-apple-darwin x86_64-apple-darwin
cd rust
cargo build --release --target aarch64-apple-darwin   # Apple Silicon
cargo build --release --target x86_64-apple-darwin    # Intel
# optional single universal binary spanning both:
lipo -create -output qmp-mcp \
  target/aarch64-apple-darwin/release/qmp-mcp \
  target/x86_64-apple-darwin/release/qmp-mcp
```

Install the runtime on macOS with `brew install qemu`. One caveat: KVM is Linux-only, so
on a Mac `accel: "auto"` resolves to TCG (there's no `/dev/kvm`) — hardware HVF
acceleration isn't a hardware-spec option yet.

**Linux ARM64** cross-builds from an x86-64 Linux host with the GNU cross linker (already
wired up in `.cargo/config.toml`):

```bash
sudo apt-get install -y gcc-aarch64-linux-gnu    # Debian/Ubuntu
rustup target add aarch64-unknown-linux-gnu
cd rust && cargo build --release --target aarch64-unknown-linux-gnu
```

CI produces the stripped x86-64 Linux binary as a downloadable artifact — automatically
on a version tag, and as a manual job on `rust`-labelled merge requests. Build the macOS
and ARM artifacts with the commands above.

## The tools

| Tool | What it does |
| --- | --- |
| `create_instance` / `destroy_instance` | build & launch a VM from a hardware spec / tear it down |
| `get_instance` / `get_status` | the current VM + lifecycle state / the live guest run state |
| `pause_instance` / `resume_instance` | freeze / unfreeze the guest CPUs |
| `reset_instance` / `powerdown_instance` | hard reset / graceful ACPI shutdown |
| `list_block_devices` / `query_cpus` | the VM's disks / per-CPU info |
| `screendump` | a PNG screenshot of the display |
| `get_events` / `wait_for_event` | recent QEMU events / block until a named one arrives |
| `qmp_execute` | a raw QMP command, gated by the command policy |
| `create_image` / `list_images` / `list_isos` | make a disk image / list disks / list boot ISOs |

## Configuration

Everything is `QMP_MCP_*` environment variables — the same names and defaults as the
TypeScript variant, and invalid values fail closed at startup with a message naming the
variable. The full, commented list lives in [`../.env.example`](../.env.example) (and the
command-policy file format in [`../policy.example.yaml`](../policy.example.yaml)). The
ones you'll reach for:

| Variable | Default | What it does |
| --- | --- | --- |
| `QMP_MCP_TRANSPORT` | `stdio` | `stdio`, `http`, or `both` (the image defaults to `http`) |
| `QMP_MCP_API_KEYS` | _(unset)_ | comma-separated API keys for the HTTP transport |
| `QMP_MCP_AUTH` / `QMP_MCP_JWT_SECRET` | `apikey` | switch to `jwt` (HS256 Bearer tokens) instead of API keys |
| `QMP_MCP_ALLOW_INSECURE` | `false` | run HTTP unauthenticated (local dev only) |
| `QMP_MCP_QEMU_BINARY` | _(derived from `machine`)_ | usually unset — the emulator is derived from the `machine` (q35→x86_64, virt/raspi*→aarch64, ADR-0013); set it to force one emulator for every Instance |
| `QMP_MCP_IMAGE_DIR` / `QMP_MCP_ISO_DIR` | XDG paths | the read-write disk folder / read-only ISO folder |
| `QMP_MCP_VIEWER_PASSWORD` | _(unset)_ | enables the noVNC viewer (required to request a `vnc` display) |
| `QMP_MCP_VIEWER_USER` | _(unset)_ | optional username enforced alongside the password on the viewer's HTTP Basic auth (default: username ignored) |
| `QMP_MCP_HOST_SHARE_DIR` | _(unset)_ | absolute host dir shared into guests via virtio-9p when a spec sets `share: true` (unset ⇒ off; ADR-0014) |
| `QMP_MCP_GUEST_SHARE_DIR` | _(unset)_ | intended guest mountpoint (advisory) — `get_share` reports the exact `mount -t 9p` command |
| `QMP_MCP_ALLOW_SHARE_WRITE` | `false` | mount the share read-write (default read-only) |
| `QMP_MCP_ALLOW_RAW_ARGS` | `false` | let a spec pass raw QEMU flags (the escape hatch) |

…plus the HTTP host/port/origins, caps on disk/memory/vCPUs, the port-forward range, the
command-policy allow/deny lists and policy file, and the event-buffer size. See
`../.env.example` for the whole list.

## Developing

Toolchains run in Docker so they never touch your host (that dev container setup is a
local, uncommitted convenience). The essentials:

```bash
cd rust
cargo build
cargo test                                             # unit + integration
cargo clippy --all-targets --all-features -- -D warnings
cargo fmt --all -- --check                             # or `cargo fmt --all` to apply
```

The deterministic argv, command policy, and store-name behaviors are cross-validated
against the language-neutral golden fixtures in [`../testdata`](../testdata) — the *same*
fixtures the TypeScript variant asserts, so drift on either side fails the build. The one
real-qemu TCG integration test and the `qemu-img create` test **skip themselves** when
those binaries aren't installed, so `cargo test` is green on a qemu-less machine and
really exercises QEMU where it exists.

## License

[MIT](../LICENSE).
