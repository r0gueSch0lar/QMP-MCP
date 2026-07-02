# qmp-mcp (Rust variant)

A secure [Model Context Protocol](https://modelcontextprotocol.io) server that lets an
AI agent build, launch, drive, and tear down a single QEMU virtual machine (the
**Instance**) through QEMU's [QMP](https://www.qemu.org/docs/master/interop/qmp-spec.html)
API. The agent never supplies raw QEMU arguments: it fills a structured, validated
**Hardware Spec** and the server generates the `qemu-system-*` argv from it.

This is the **Rust variant** — a second implementation of the same bounded context as
the TypeScript server in [`../src`](../src), built on the official MCP Rust SDK
([`rmcp`](https://docs.rs/rmcp) 0.16) + [tokio](https://tokio.rs), and targeting full
behavioral parity (same spec → same argv, same Command Policy verdicts, same
fail-closed behavior). See [`../CONTEXT.md`](../CONTEXT.md) for the domain glossary
(Instance, Guest, QMP Session, Hardware Spec, Command Policy, Image/ISO Store, Event
Buffer, Display, Viewer) and [`../docs/adr/`](../docs/adr) for the architectural
decisions — [ADR-0011](../docs/adr/0011-rust-variant-rmcp.md) records the Rust stance,
[ADR-0012](../docs/adr/0012-parity-golden-fixtures.md) the shared parity fixtures.

The crate and binary are both named `qmp-mcp`. It runs equally as an ordinary process
on a host with QEMU installed (stdio) **and** as a container exposing the HTTP
transport — bare metal is a co-equal target (ADR-0007). It runs **non-root** in both
modes and never needs `--privileged` (ADR-0008).

## Requirements

- **Rust** 1.96+ (edition 2021) to build.
- **QEMU** at runtime: `qemu-system-x86_64` and `qemu-img` on `PATH` (the default
  Hardware Spec targets x86; `create_image` shells out to `qemu-img`).
- **Platforms:** builds and runs on Linux (x86-64, ARM64) and macOS (Intel &
  Apple Silicon) — see [Building for other platforms](#building-for-other-platforms-cross-compilation).

## Build

From the crate directory (`rust/`):

```bash
cargo build --release        # -> target/release/qmp-mcp
cargo run --release           # run it directly (defaults to the stdio transport)
```

This repo develops the toolchains in Docker (see [`../CONTEXT.md`](../CONTEXT.md) and
the git-excluded `compose.yaml`); the same commands run inside the `rust-dev`
container:

```bash
docker compose exec -T rust-dev cargo build --release
docker compose exec -T rust-dev cargo test
docker compose exec -T rust-dev cargo clippy --all-targets --all-features -- -D warnings
docker compose exec -T rust-dev cargo fmt --all -- --check
```

## Building for other platforms (cross-compilation)

The server is portable Rust and builds for Linux and macOS. The default target is
x86-64 Linux; pass `--target <triple>` for the rest, running cargo from the crate
directory (`rust/`) so the per-target linker settings in
[`.cargo/config.toml`](./.cargo/config.toml) apply. Add a target's std once with
`rustup target add <triple>`. Every build needs `qemu-system-*` + `qemu-img` on
`PATH` at **runtime** on the host it runs on.

| Target triple | Platform | Build on | How |
| --- | --- | --- | --- |
| `x86_64-unknown-linux-gnu` | Linux x86-64 (default) | Linux | `cargo build --release` |
| `aarch64-unknown-linux-gnu` | Linux ARM64 | Linux (cross) | see below |
| `aarch64-apple-darwin` | macOS Apple Silicon | macOS | see below |
| `x86_64-apple-darwin` | macOS Intel | macOS | see below |

Release binaries are stripped (`[profile.release] strip = true`).

### macOS

macOS binaries must be built **on macOS** — the Apple SDK + linker are required, and
cross-building them from Linux needs osxcross (out of scope). No linker override is
needed on a Mac. On a Mac with the Rust toolchain:

```bash
rustup target add aarch64-apple-darwin x86_64-apple-darwin
cd rust
cargo build --release --target aarch64-apple-darwin   # Apple Silicon
cargo build --release --target x86_64-apple-darwin    # Intel
# optional single universal (fat) binary spanning both Mac arches:
lipo -create -output qmp-mcp \
  target/aarch64-apple-darwin/release/qmp-mcp \
  target/x86_64-apple-darwin/release/qmp-mcp
```

Install the runtime with `brew install qemu` (`qemu-system-*` + `qemu-img`). Note:
KVM is Linux-only, so `accel: "auto"` resolves to **TCG** on macOS (there is no
`/dev/kvm`); hardware **HVF** acceleration is not yet a Hardware Spec option.

### Linux ARM64 (cross-build from x86-64)

Install the GNU cross linker (already wired in `.cargo/config.toml`), add the
target, and build:

```bash
sudo apt-get install -y gcc-aarch64-linux-gnu    # Debian/Ubuntu
rustup target add aarch64-unknown-linux-gnu
cd rust && cargo build --release --target aarch64-unknown-linux-gnu
```

### CI

The `rust-release-linux-amd64` job (in [`../.gitlab-ci.yml`](../.gitlab-ci.yml))
produces the stripped x86-64 Linux binary as a downloadable artifact — automatically
on a version tag, and as a **manual** job on `rust`-labelled MRs. macOS and ARM
artifacts are built with the commands above (on a Mac / macOS runner, or a Linux ARM
cross-build).

## Install (the `cargo install` "npx-equivalent")

The crate is not published to a registry; install the `qmp-mcp` binary from source with
`cargo install`. This is the Rust analogue of the TypeScript server's `npx -y qmp-mcp`:
it compiles once and drops `qmp-mcp` onto your `PATH` (`~/.cargo/bin`).

```bash
# From a checkout of this repo (the crate lives in the rust/ subdirectory):
cargo install --locked --path rust        # run from the repo root
# …or, from inside rust/:
cargo install --locked --path .
```

`--locked` honors the committed `Cargo.lock` for a reproducible build. Then just run
`qmp-mcp` (stdio by default). Point an MCP client at it directly:

```json
{
  "mcpServers": {
    "qmp": {
      "command": "qmp-mcp",
      "env": {
        "QMP_MCP_IMAGE_DIR": "/srv/qmp/images",
        "QMP_MCP_ISO_DIR": "/srv/qmp/isos"
      }
    }
  }
}
```

## Run

The transport is selected by `QMP_MCP_TRANSPORT` (`stdio` | `http` | `both`).

### stdio (bare metal, default)

```bash
qmp-mcp                       # or: cargo run --release
```

This speaks newline-delimited JSON-RPC over stdio — the shape MCP clients launch
directly. No network port, so it is auth-free.

### HTTP

```bash
QMP_MCP_TRANSPORT=http \
QMP_MCP_API_KEYS=replace-with-a-strong-key \
  qmp-mcp
```

The HTTP transport **fails closed** (ADR-0005): it refuses to start unless you provide
auth — `QMP_MCP_API_KEYS=...` (sent in the `X-API-Key` header). For throwaway local use
you can opt out with `QMP_MCP_ALLOW_INSECURE=true`, but never expose that on an
untrusted network. (`QMP_MCP_AUTH=jwt` is defined by the shared config surface but not
yet implemented in this variant — it fails closed with an actionable message; use
`apikey` or the insecure override.) `QMP_MCP_TRANSPORT=both` serves stdio and HTTP
concurrently.

## Run with Docker

The Rust image ([`Dockerfile`](./Dockerfile)) is a `cargo-chef` multi-stage build →
a slim `debian:bookworm-slim` runtime that ships `qemu-system-x86` + `qemu-utils`, runs
as a non-root user, and defaults to the **HTTP** transport bound to `0.0.0.0` so a
published port is reachable. The noVNC Viewer assets are embedded in the binary, so the
runtime image copies exactly one file — the compiled server.

The image is tagged **distinctly from the TypeScript image** (`qmp-mcp-rs` / the
`qmp-mcp:rust` tag used below) so the two never collide. The build context is the crate
directory, which is self-contained:

```bash
docker build -f rust/Dockerfile -t qmp-mcp:rust rust/

docker run --rm -p 8080:8080 \
  -e QMP_MCP_API_KEYS=replace-with-a-strong-key \
  qmp-mcp:rust
```

Persist the Stores by mounting the container's Store directories:

```bash
docker run --rm -p 8080:8080 \
  -e QMP_MCP_API_KEYS=replace-with-a-strong-key \
  -v qmp-images:/var/lib/qmp-mcp/images \
  -v qmp-isos:/var/lib/qmp-mcp/isos \
  qmp-mcp:rust
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
  qmp-mcp:rust
```

`accel: "auto"` (the Hardware Spec default) probes `/dev/kvm` and uses KVM when it is
accessible, otherwise falls back to TCG, reporting which it chose. `accel: "kvm"`
hard-fails with an actionable message when `/dev/kvm` is unavailable.

## Browser Viewer (noVNC)

The **Display** is the Guest's graphical output; the **Viewer** is an optional
in-process [noVNC](https://novnc.com) bridge that lets you watch and control it in a
browser (ADR-0010). Its assets are embedded in the binary (`include_dir!`) — no runtime
asset directory, traversal-safe by construction. It is off by default and interactive
(keyboard + mouse), not view-only.

- Set `QMP_MCP_VIEWER_PASSWORD` to a strong password. Without it, requesting a `vnc`
  Display **rejects** `create_instance` with an actionable error — the Viewer is
  fail-closed and refuses to serve without its own password.
- Call `create_instance` with `display: "vnc"` in the Hardware Spec. The server
  attaches a **loopback-only** VNC server to the Guest, arms it with an internal
  password over QMP (never in the process list), and starts the Viewer for the lifetime
  of that Instance. `destroy_instance` stops it.
- Open `http://<host>:6080/` in a browser (the Viewer runs on its own HTTP server,
  independent of the MCP transport — it works even under `stdio`). Authenticate with
  `QMP_MCP_VIEWER_PASSWORD` (HTTP Basic; any username).

With Docker, publish the Viewer port alongside the MCP port and pass the password:

```bash
docker run --rm -p 8080:8080 -p 6080:6080 \
  -e QMP_MCP_API_KEYS=replace-with-a-strong-key \
  -e QMP_MCP_VIEWER_PASSWORD=replace-with-a-strong-viewer-password \
  qmp-mcp:rust
```

The raw VNC port (5900) is **loopback-only and never published**. The Viewer serves
plain HTTP, so on a non-loopback bind (the image sets `0.0.0.0`) it logs a startup
**WARNING** — front it with a TLS-terminating reverse proxy on any untrusted network.
It also refuses to be framed, rejects cross-origin websocket upgrades, and caps
concurrent connections.

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

Configured entirely through `QMP_MCP_*` environment variables — the **same names,
defaults, and validation** as the TypeScript server. Invalid values **fail closed** at
startup with a message naming the variable and its allowed values. The exhaustive,
commented reference lives in the shared [`../.env.example`](../.env.example) and, for
the Command Policy, [`../policy.example.yaml`](../policy.example.yaml).

| Variable | Default | Purpose |
| --- | --- | --- |
| `QMP_MCP_TRANSPORT` | `stdio` | Transport: `stdio` \| `http` \| `both`. (The image overrides to `http`.) |
| `QMP_MCP_LOG_LEVEL` | `info` | Logger verbosity: `debug` \| `info` \| `warning` \| `error`. |
| `QMP_MCP_HTTP_HOST` | `127.0.0.1` | HTTP bind address. (The image overrides to `0.0.0.0`.) |
| `QMP_MCP_HTTP_PORT` | `8080` | HTTP listen port. |
| `QMP_MCP_HTTP_ENDPOINT` | `/mcp` | HTTP MCP endpoint path. |
| `QMP_MCP_HTTP_ALLOWED_ORIGINS` | loopback origins | Comma-separated browser origins for the DNS-rebinding/CORS guard. |
| `QMP_MCP_AUTH` | `apikey` | HTTP auth provider: `apikey` \| `jwt` (`jwt` not yet implemented in this variant). |
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

## Development

```bash
cargo build                                          # debug build
cargo test                                            # unit + integration tests
cargo clippy --all-targets --all-features -- -D warnings
cargo fmt --all -- --check                            # or `cargo fmt --all` to apply
```

The deterministic argv, Command Policy, and Store-name behaviors are cross-validated
against the language-neutral golden fixtures in [`../testdata`](../testdata), which
**both** implementations assert (ADR-0012) — any unintentional drift fails the fixture.
The single real-qemu TCG integration test and the `qemu-img create` test **runtime-skip**
when those binaries are absent, so `cargo test` is green on a qemu-less machine (e.g.
the CI toolchain image) and really exercises qemu where it exists (the `rust-dev`
container).
