# Purpose-built distro image; bare metal is a co-equal target; qemux extension deferred

There is no official QEMU image from the QEMU project. We build a purpose-fit image:
a multi-stage Dockerfile that compiles the MCP server in a Node stage and ships a
final `debian:stable-slim` stage with `qemu-system-x86` + `qemu-utils` + a Node
runtime, with our server as `ENTRYPOINT`. We do not extend `qemux/qemu`, whose
entrypoint is designed to boot its own VM and would conflict with our orchestrator
owning the Instance lifecycle.

**Bare metal is a co-equal deployment target.** The server must run as an ordinary
process via `npx`/`node` on a host that has QEMU installed, with no dependency on the
Docker filesystem layout. Config defaults are therefore host-agnostic and overridable;
the Docker image supplies the container-specific paths (e.g. `/var/lib/qmp-mcp/...`)
via env, rather than those paths being baked in as the only option.

Extending the `qemux/qemu` community edition is recorded as a future option and will be
explored in a separate design note (`docs/qemux-integration.md`) — what it offers (tuned
KVM/networking, web viewer) and what overriding its entrypoint to use it purely as a
QEMU binary provider would entail.
