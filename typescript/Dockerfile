# syntax=docker/dockerfile:1
#
# Purpose-built qmp-mcp image (ADR-0007): a Node build stage compiles the server,
# and a slim Debian+Node runtime stage ships qemu-system (all architectures) +
# qemu-img alongside the production deps and the compiled dist, running our server as a
# NON-ROOT user (ADR-0008). The image never needs --privileged; /dev/kvm is an
# OPTIONAL performance upgrade (pass `--device /dev/kvm` and the kvm group), and it
# falls back to TCG software emulation otherwise.
#
# This is NOT Dockerfile.dev (the git-excluded local dev container); it is the
# distributable production image.

# ── Build stage: compile TypeScript -> dist/ ────────────────────────────────────
# Node 22 LTS on Debian trixie (13). Trixie ships QEMU 10.0.x for the runtime stage
# (issue #31, 7.2 → 11 audit); Node 22 replaces the now-EOL Node 20.
FROM node:22-trixie-slim AS build
WORKDIR /app

# Activate pnpm via corepack, pinned by the `packageManager` field in package.json.
RUN corepack enable

# Install ALL dependencies (including the dev toolchain) from the lockfile for a
# reproducible build. Copy only the manifests first so this layer caches until the
# dependency set actually changes. pnpm-workspace.yaml carries the `allowBuilds`
# opt-in without which pnpm's pre-script deps check fails on esbuild's build script.
COPY package.json pnpm-lock.yaml pnpm-workspace.yaml ./
RUN pnpm install --frozen-lockfile

# Compile the server (tsc -> dist/). Only the inputs the build needs are copied.
COPY tsconfig.json ./
COPY src ./src
RUN pnpm run build

# ── Runtime stage: QEMU + Node, production deps only, non-root ───────────────────
# Same base as the build stage: Node 22 LTS on Debian trixie, whose `qemu-system`
# metapackage provides QEMU 10.0.x (issue #31). Still distro QEMU, not source-built
# (ADR-0007).
FROM node:22-trixie-slim AS runtime

# QEMU full-system emulators (all architectures) + qemu-img, from Debian. The
# `qemu-system` metapackage ships every qemu-system-* (arm/aarch64, x86, ppc, mips,
# sparc, riscv/s390x, ...) so QMP_MCP_QEMU_BINARY can select any guest architecture
# the Hardware Spec supports — e.g. `qemu-system-aarch64` for the raspi* boards.
# There is no official QEMU image (ADR-0007). Clean the apt lists in the same layer.
RUN apt-get update \
  && apt-get install -y --no-install-recommends \
       qemu-system \
       qemu-utils \
  && rm -rf /var/lib/apt/lists/*

ENV NODE_ENV=production
WORKDIR /app

# Activate pnpm via corepack (pinned by the `packageManager` field), matching the
# build stage's toolchain.
RUN corepack enable

# Production dependencies ONLY — the dev toolchain (tsc, vitest, biome) never ships
# in the runtime image. The lockfile pins the same versions resolved at build time.
COPY package.json pnpm-lock.yaml pnpm-workspace.yaml ./
RUN pnpm install --prod --frozen-lockfile

# The compiled server. package.json is already present above, so the runtime
# version lookup (`../package.json` relative to dist/index.js) resolves correctly.
COPY --from=build /app/dist ./dist

# Container-specific Store paths (ADR-0006/0007). These are supplied via env rather
# than baked into the code, so the host-agnostic bare-metal defaults are preserved
# for non-Docker deployments. The directories are created and owned by the non-root
# user below.
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

USER qmp

# 8080: the MCP HTTP transport. 6080: the optional noVNC Viewer (ADR-0010), served
# only while a `display: vnc` Instance runs and only behind QMP_MCP_VIEWER_PASSWORD.
EXPOSE 8080 6080

# `node dist/index.js` is the `qmp-mcp` bin. Our orchestrator owns the Instance
# lifecycle, so the entrypoint is the server itself — never a VM-booting wrapper
# (the reason we do not extend an existing QEMU image, ADR-0007).
ENTRYPOINT ["node", "dist/index.js"]
