# syntax=docker/dockerfile:1
#
# Purpose-built qmp-mcp Rust image (ADR-0007/0011): a cargo-chef multi-stage build
# compiles the `qmp-mcp` binary, and a slim Debian runtime stage ships
# qemu-system-x86 + qemu-img alongside the statically-embedded server, running as a
# NON-ROOT user (ADR-0008). The image never needs --privileged; /dev/kvm is an
# OPTIONAL performance upgrade (pass `--device /dev/kvm` and the kvm group), and it
# falls back to TCG software emulation otherwise.
#
# The noVNC Viewer assets are EMBEDDED in the binary at compile time
# (`include_dir!`, ADR-0010/0011), so the runtime stage copies exactly one file —
# the binary — and needs no asset directory.
#
# This mirrors the security posture of the production TypeScript image (../Dockerfile):
# a non-root system user, a minimal runtime with only qemu-system-x86 + qemu-utils,
# container-specific Store paths supplied via env, and the HTTP transport failing
# closed until an operator supplies auth. The Rust image is tagged DISTINCTLY from
# the TS image (e.g. `qmp-mcp:rust` / `qmp-mcp-rs`) so the two never collide.
#
# Build context is the crate directory (`rust/`), which is self-contained: the only
# compile-time file inputs are `assets/novnc/` (embedded) and Cargo.{toml,lock}.
#   docker build -f rust/Dockerfile -t qmp-mcp:rust rust/
#
# This is NOT Dockerfile.rust-dev (the git-excluded local dev container); it is the
# distributable production image.

# ── Chef base: the Rust toolchain + cargo-chef ──────────────────────────────────
# `rust:1-bookworm` matches the dev toolchain (1.96) and the CI fmt/clippy/test
# image, so the image builds with the exact compiler the checks ran against.
FROM rust:1-bookworm AS chef
# `--locked` installs cargo-chef against its own pinned lockfile for a reproducible
# planner/cook toolchain. cargo-chef splits dependency compilation from the app
# build so the (slow) dependency layer caches until Cargo.toml/Cargo.lock change.
RUN cargo install cargo-chef --locked
WORKDIR /app

# ── Planner: distill the dependency graph into a recipe ──────────────────────────
# `cargo chef prepare` reads only the manifests and lockfile, so this layer is a
# pure function of the dependency set and reruns only when it changes.
FROM chef AS planner
COPY . .
RUN cargo chef prepare --recipe-path recipe.json

# ── Builder: cook dependencies (cached), then compile the binary ─────────────────
FROM chef AS builder
COPY --from=planner /app/recipe.json recipe.json
# Compile ONLY the dependencies from the recipe. This heavy layer is cached and
# reused across builds until the recipe (i.e. the dependency set) actually changes.
RUN cargo chef cook --release --recipe-path recipe.json
# Now the real source (including assets/novnc for the embedded Viewer). With deps
# already cooked, this final step recompiles only the crate itself.
COPY . .
RUN cargo build --release --bin qmp-mcp

# ── Runtime: slim Debian + QEMU, non-root ────────────────────────────────────────
FROM debian:bookworm-slim AS runtime

# QEMU x86 system emulator + qemu-img, from Debian. There is no official QEMU image
# (ADR-0007), so we install exactly what the server launches and probes. Clean the
# apt lists in the same layer to keep the image lean. ca-certificates covers any
# outbound TLS the runtime might need.
RUN apt-get update \
  && apt-get install -y --no-install-recommends \
       qemu-system-x86 \
       qemu-utils \
       ca-certificates \
  && rm -rf /var/lib/apt/lists/*

# Container-specific Store paths (ADR-0006/0007). Supplied via env rather than baked
# into the code, so the host-agnostic bare-metal defaults are preserved for non-Docker
# deployments. The directories are created and owned by the non-root user below.
ENV QMP_MCP_IMAGE_DIR=/var/lib/qmp-mcp/images \
    QMP_MCP_ISO_DIR=/var/lib/qmp-mcp/isos

# Default to the HTTP transport bound to all interfaces so a published port is
# reachable from outside the container. Auth still FAILS CLOSED (ADR-0005): the
# server refuses to start unless you pass `-e QMP_MCP_API_KEYS=...` (or a JWT
# secret), or explicitly opt out with QMP_MCP_ALLOW_INSECURE=true (local dev only).
ENV QMP_MCP_TRANSPORT=http \
    QMP_MCP_HTTP_HOST=0.0.0.0 \
    QMP_MCP_HTTP_PORT=8080

# The optional noVNC Viewer (ADR-0010) runs on its own HTTP server. Bind it to all
# interfaces so a published port is reachable; it stays FAIL-CLOSED behind
# QMP_MCP_VIEWER_PASSWORD (unset here — a vnc Display is refused until you set it).
ENV QMP_MCP_VIEWER_HOST=0.0.0.0 \
    QMP_MCP_VIEWER_PORT=6080

# Run as a dedicated non-root user (ADR-0008). TCG works rootless with zero device
# access, so the zero-privilege path always works; KVM is opt-in only.
RUN groupadd --system qmp \
  && useradd --system --gid qmp --home-dir /home/qmp --create-home qmp \
  && mkdir -p "${QMP_MCP_IMAGE_DIR}" "${QMP_MCP_ISO_DIR}" \
  && chown -R qmp:qmp /var/lib/qmp-mcp

# The compiled server — the single artifact the runtime needs (the noVNC assets are
# embedded in it). Placed on PATH so the entrypoint is just its name.
COPY --from=builder /app/target/release/qmp-mcp /usr/local/bin/qmp-mcp

USER qmp

# 8080: the MCP HTTP transport. 6080: the optional noVNC Viewer (ADR-0010), served
# only while a `display: vnc` Instance runs and only behind QMP_MCP_VIEWER_PASSWORD.
EXPOSE 8080 6080

# The `qmp-mcp` binary is the server itself. Our orchestrator owns the Instance
# lifecycle, so the entrypoint is the server — never a VM-booting wrapper (the reason
# we do not extend an existing QEMU image, ADR-0007).
ENTRYPOINT ["qmp-mcp"]
