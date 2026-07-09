# Guest folder sharing via virtio-9p; operator-configured host path, agent opts in

A host directory can be shared into the guest over **virtio-9p** (`-fsdev local` +
`-device virtio-9p-pci`, `security_model=mapped-xattr`). virtio-9p is chosen over
virtiofs and user-mode SMB because it is the only option that needs **zero extra host
binaries** — it is in-tree in `qemu-system-*`, so `buildArgv` stays a pure, deterministic
function of the Hardware Spec (virtiofs needs an out-of-process `virtiofsd` daemon plus a
shared-memory backing for all guest RAM; SMB shells out to the host `smbd`). Both of those
would break the single-static-server, qemu-only-image posture.

The trust split mirrors the Image/ISO Stores: **the host path is operator-only.**
`QMP_MCP_HOST_SHARE_DIR` (an absolute host path, validated fail-closed) is the single
shared directory; unset means sharing is disabled. The **agent's only lever is a boolean**
`share: true` on the Hardware Spec — it carries no path and no tag, so it can never name,
inject, or exfiltrate a host directory; it can only opt in to the pre-configured share.
The mount tag is the fixed server constant `share`. The share is **read-only** unless the
operator sets `QMP_MCP_ALLOW_SHARE_WRITE=true` (same fail-closed shape as
`QMP_MCP_ALLOW_HOST_NET`) — the agent can never escalate to writable. The host path is
comma-escaped into the argv, and sharing is refused on the `raspi*` boards, which have no
PCI bus (exactly like a PCI NIC). Other PCI-less machines (`microvm`, `isapc`) are likewise
unsupported for sharing — the same pre-existing bus-model limitation the default PCI NIC
already has; the common targets (`q35`/`pc`/`virt`) all have PCI.

`QMP_MCP_GUEST_SHARE_DIR` is **advisory**: QEMU physically cannot mount inside the guest —
9p only carries the tag, and the guest OS runs the mount. So this env var is the *intended*
guest mountpoint, surfaced by the read-only `get_share` tool along with the exact
`mount -t 9p -o trans=virtio,version=9p2000.L[,ro] share <mountpoint>` command. Operators
who want it to appear automatically bake that line into their guest image's fstab; the
server never injects fstab/cloud-init (it does not own the guest image, ADR-0007).

We record this because a host bind-mount is the most dangerous capability to hand an
agent, and two design choices are load-bearing and easy to get wrong later:

1. **Opt-in is a boolean, not a path or a tool.** A tool (or spec field) that accepted a
   host path would re-open full host read/write to the agent. A *report* tool (`get_share`)
   is safe and useful; a *configure*/*mount* tool is not.
2. **A runtime "attach a share to the running VM" tool is infeasible by design.** 9p/
   virtiofs hotplug requires `device_add`/`chardev-add`/`object-add`, all on the immutable
   Command Policy hard-denylist (ADR-0003) that can never be re-enabled. Sharing is
   therefore create-time only (via `share: true` → argv), which is also where it belongs.
