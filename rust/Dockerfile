# syntax=docker/dockerfile:1
#
# Purpose-built qmp-mcp Rust image (ADR-0007/0011): a cargo-chef multi-stage build
# compiles the `qmp-mcp` binary, and a slim Debian runtime stage ships
# qemu-system (all architectures) + qemu-img alongside the statically-embedded
# server, running as a NON-ROOT user (ADR-0008). The image never needs --privileged; /dev/kvm is an
# OPTIONAL performance upgrade (pass `--device /dev/kvm` and the kvm group), and it
# falls back to TCG software emulation otherwise.
#
# The noVNC Viewer assets are EMBEDDED in the binary at compile time
# (`include_dir!`, ADR-0010/0011), so the runtime stage copies exactly one file —
# the binary — and needs no asset directory.
#
# This mirrors the security posture of the production TypeScript image (../Dockerfile):
# a non-root system user, a minimal runtime with qemu-system (all archs) + qemu-utils,
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
# `rust:1-trixie` pins the latest stable 1.x compiler on Debian trixie, matching
# the runtime stage's glibc so the binary is built and run against the same libc
# (no forward-compat gap). CI runs fmt/clippy/test via `dtolnay/rust-toolchain@stable`,
# i.e. the same stable channel this image tracks.
FROM rust:1-trixie AS chef
# `--locked` installs cargo-chef against its own pinned lockfile for a reproducible
# planner/cook toolchain. cargo-chef splits dependency compilation from the app
# build so the (slow) dependency layer caches until Cargo.toml/Cargo.lock change.
# The registry/git BuildKit cache mounts persist crate downloads across builds;
# the compiled cargo-chef binary itself lands in /usr/local/cargo/bin (a real layer).
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    cargo install cargo-chef --locked
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
# Compile ONLY the dependencies from the recipe. This step is layer-cached until
# the recipe (i.e. the dependency set) changes, and the BuildKit cache mounts keep
# the crate downloads and dep artifacts across builds even when it does — a recipe
# bump then recompiles only the deps that actually changed, not the whole set.
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    --mount=type=cache,target=/app/target \
    cargo chef cook --release --recipe-path recipe.json
# Now the real source (including assets/novnc for the embedded Viewer). With deps
# already cooked, this final step recompiles only the crate itself. target/ is a
# cache mount (invisible to COPY --from), so the binary is copied out to a real
# layer path in the same RUN.
COPY . .
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    --mount=type=cache,target=/app/target \
    cargo build --release --bin qmp-mcp \
  && cp target/release/qmp-mcp /app/qmp-mcp

# ── Runtime: slim Debian + QEMU, non-root ────────────────────────────────────────
# Debian trixie (13) ships QEMU 10.0.x, well clear of the deprecations audited in
# issue #31 (7.2 → 11). Still distro-packaged QEMU, not source-built (ADR-0007).
FROM debian:trixie-slim AS runtime

# QEMU full-system emulators (all architectures) + qemu-img, from Debian. The
# `qemu-system` metapackage ships every qemu-system-* (arm/aarch64, x86, ppc, mips,
# sparc, riscv/s390x, ...) so QMP_MCP_QEMU_BINARY can select any guest architecture
# the Hardware Spec supports — e.g. `qemu-system-aarch64` for the raspi* boards.
# There is no official QEMU image (ADR-0007); ca-certificates covers any outbound
# TLS the runtime needs. The .deb downloads and index lists live in BuildKit cache
# mounts (sharing=locked serializes dpkg across concurrent builds), so nothing needs
# cleaning out of the layer and repeat builds skip the downloads. Debian's stock
# docker-clean hook would delete the cached .debs, so it is disabled first.
RUN rm -f /etc/apt/apt.conf.d/docker-clean \
  && echo 'Binary::apt::APT::Keep-Downloaded-Packages "true";' > /etc/apt/apt.conf.d/keep-cache
RUN --mount=type=cache,target=/var/cache/apt,sharing=locked \
    --mount=type=cache,target=/var/lib/apt,sharing=locked \
    apt-get update \
  && apt-get install -y --no-install-recommends \
       qemu-system \
       qemu-utils \
       ca-certificates \
       ffmpeg

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
# embedded in it). Placed on PATH so the entrypoint is just its name. /app/qmp-mcp
# is where the builder copied it out of the target/ cache mount.
COPY --from=builder /app/qmp-mcp /usr/local/bin/qmp-mcp

# The built-in ISO download catalog (ADR-0018) is EMBEDDED in the binary, so the server needs
# no runtime file; this copy is a convenience so an operator can inspect/copy the shipped list
# (it is also the path QMP_MCP_ISO_CATALOG points at in .env.example).
COPY --from=builder /app/assets/iso-catalog.json /usr/local/share/qmp-mcp/iso-catalog.json

USER qmp

# 8080: the MCP HTTP transport. 6080: the optional noVNC Viewer (ADR-0010), served
# only while a `display: vnc` Instance runs and only behind QMP_MCP_VIEWER_PASSWORD.
EXPOSE 8080 6080

# The `qmp-mcp` binary is the server itself. Our orchestrator owns the Instance
# lifecycle, so the entrypoint is the server — never a VM-booting wrapper (the reason
# we do not extend an existing QEMU image, ADR-0007).
ENTRYPOINT ["qmp-mcp"]
