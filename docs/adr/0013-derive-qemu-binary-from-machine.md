# Derive the QEMU binary from the machine; gate KVM on a guest/host arch match

The `qemu-system-*` binary is chosen per-Instance from the Hardware Spec's `machine`,
not from a single server-wide setting. `q35`/`pc` launch `qemu-system-x86_64`;
`virt`/`sbsa-ref` and every `raspi*` board launch `qemu-system-aarch64` (a superset
emulator that hosts the 32-bit raspi boards too). An unrecognized machine falls back to
`qemu-system-x86_64`, the historical default. `QMP_MCP_QEMU_BINARY`, previously the only
selector, becomes an optional **override**: when set (to a bare command name or absolute
path) it is honored for every Instance — e.g. a custom-built emulator — and when unset the
binary is derived from `machine`.

`accel: auto` (ADR-0008) now chooses KVM only when it is actually viable: the guest
architecture — read from the **launched binary** (`qemu-system-<arch>`), so an override of
a different arch than the machine is respected — must match the host, and the machine must
not be a fixed-CPU `raspi*` board (KVM can't virtualize their baked CPU). KVM cannot cross
architectures, so an aarch64 `virt` guest on an x86_64 host, or any raspi board, resolves
to TCG rather than failing with QEMU's "invalid accelerator kvm". Explicit `kvm`/`tcg` are
unchanged. One residual case `auto` does not gate: a named CPU (e.g. `cortex-a72`) on a
*matching* ARM host, where ARM KVM requires `host`/`max` — that surfaces as a QEMU launch
error, and the fix is an explicit `accel: tcg`.

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
