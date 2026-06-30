# Single-instance QEMU lifecycle orchestrator

The server orchestrates the full lifecycle of **one** QEMU Instance at a time —
build → run → drive via QMP → destroy — rather than being a thin QMP bridge to an
externally-started QEMU, or a multi-VM orchestrator.

We chose this because the goal is agent-driven VM setup with user-defined hardware:
a pure bridge could not build VMs, and multi-VM orchestration would add an instance
registry plus per-instance port/socket allocation and resource bookkeeping that the
single-instance use case does not need. Multi-VM and external-attach remain possible
future extensions, but are explicitly out of scope for now.
