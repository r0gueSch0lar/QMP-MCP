# Guest networking is user-mode by default; host-level networking is env-gated off

Guest NICs default to QEMU user-mode networking (`-netdev user`, SLiRP): NAT'd outbound
with the host network not exposed, and inbound only via explicit port-forwards. Those
forwards are limited to a configurable host-port range so the agent cannot bind
arbitrary or privileged host ports. tap/bridge networking (`-netdev tap`) is disabled
unless an env flag (e.g. `QMP_MCP_ALLOW_HOST_NET=true`) enables it, and is documented as
an advanced, privileged option.

We chose user-mode by default because tap/bridge puts the guest directly on the host LAN
and needs `CAP_NET_ADMIN`/root or a setuid helper — incompatible with the unprivileged,
non-root posture (ADR-0008). User-mode gives the guest working outbound connectivity
with zero host network exposure and no privileges. This is the networking counterpart to
the storage boundary (ADR-0006): rich-but-bounded by default, full power only on a
deliberate opt-in.
