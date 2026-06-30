# Hardware is specified via a validated Hardware Spec; raw args gated off by default

The agent never supplies raw QEMU argv. It fills a structured, validated **Hardware
Spec**, and the server generates the `qemu-system-*` argv from it. A raw `extraArgs`
escape hatch exists but is disabled unless an env flag (e.g. `QMP_MCP_ALLOW_RAW_ARGS`)
turns it on.

We chose this because raw QEMU arguments are host-compromise-equivalent: `-drive
file=/etc/shadow`, `-fw_cfg file=…`, and host `-netdev` backends let a guest builder
read/write arbitrary host files and reach the host network. A safe-by-construction
spec turns "the agent picks hardware" into a bounded, auditable choice, while the
gated escape hatch preserves full QEMU power for operators who knowingly opt in on a
trusted single-tenant host.
