# syntax=docker/dockerfile:1
#
# Purpose-built qmp-mcp image (ADR-0007): a Node build stage compiles the server,
# and a slim Debian+Node runtime stage ships qemu-system-x86 + qemu-img alongside
# the production-only dependencies and the compiled dist, running our server as a
# NON-ROOT user (ADR-0008). The image never needs --privileged; /dev/kvm is an
# OPTIONAL performance upgrade (pass `--device /dev/kvm` and the kvm group), and it
# falls back to TCG software emulation otherwise.
#
# This is NOT Dockerfile.dev (the git-excluded local dev container); it is the
# distributable production image.

# ── Build stage: compile TypeScript -> dist/ ────────────────────────────────────
FROM node:20-bookworm-slim AS build
WORKDIR /app

# Install ALL dependencies (including the dev toolchain) from the lockfile for a
# reproducible build. Copy only the manifests first so this layer caches until the
# dependency set actually changes.
COPY package.json package-lock.json ./
RUN npm ci

# Compile the server (tsc -> dist/). Only the inputs the build needs are copied.
COPY tsconfig.json ./
COPY src ./src
RUN npm run build

# ── Runtime stage: QEMU + Node, production deps only, non-root ───────────────────
FROM node:20-bookworm-slim AS runtime

# QEMU x86 system emulator + qemu-img, from Debian. There is no official QEMU image
# (ADR-0007), so we install exactly what the server launches and probes. Clean the
# apt lists in the same layer to keep the image lean.
RUN apt-get update \
  && apt-get install -y --no-install-recommends \
       qemu-system-x86 \
       qemu-utils \
  && rm -rf /var/lib/apt/lists/*

ENV NODE_ENV=production
WORKDIR /app

# Production dependencies ONLY — the dev toolchain (tsc, vitest, biome) never ships
# in the runtime image. The lockfile pins the same versions resolved at build time.
COPY package.json package-lock.json ./
RUN npm ci --omit=dev && npm cache clean --force

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
