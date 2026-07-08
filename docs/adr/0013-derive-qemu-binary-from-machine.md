# Derive the QEMU binary from the machine; gate KVM on a guest/host arch match

The `qemu-system-*` binary is chosen per-Instance from the Hardware Spec's `machine`,
not from a single server-wide setting. `q35`/`pc` launch `qemu-system-x86_64`;
`virt`/`sbsa-ref` and every `raspi*` board launch `qemu-system-aarch64` (a superset
emulator that hosts the 32-bit raspi boards too). An unrecognized machine falls back to
`qemu-system-x86_64`, the historical default. `QMP_MCP_QEMU_BINARY`, previously the only
selector, becomes an optional **override**: when set (to a bare command name or absolute
path) it is honored for every Instance — e.g. a custom-built emulator — and when unset the
binary is derived from `machine`.

`accel: auto` (ADR-0008) additionally requires the guest architecture (derived from the
same machine map) to match the host architecture before it will choose KVM. KVM cannot
cross architectures, so an aarch64 `virt` guest on an x86_64 host resolves to TCG rather
than failing with QEMU's "invalid accelerator kvm". Explicit `kvm`/`tcg` are unchanged.

We record this because the `machine` field already fixes the guest architecture, so a
separate global binary selector was a latent foot-gun: the two could disagree, and
switching guest architectures meant flipping an env var and restarting the server (and
remembering `accel: tcg` for the cross-arch case). Deriving both the binary and
KVM-eligibility from `machine` removes that coupling — set `machine` and it just works.

The alternative — an explicit per-Instance `qemuBinary` spec field — was rejected for now
because it lets an untrusted MCP client name an arbitrary executable to spawn (a
privilege-escalation surface). The operator override stays env-side and trusted; moving
binary selection into the spec would first need an allowlist (mirroring the raw-args and
Command Policy gates).
