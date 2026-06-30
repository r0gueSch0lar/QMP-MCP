# Unprivileged by default; KVM optional via /dev/kvm; auto-accel falls back to TCG

The server runs as a non-root user in both Docker and bare-metal modes. The accelerator
is a Hardware Spec field defaulting to `auto`: the server probes whether `/dev/kvm` is
present and accessible, uses KVM when it is, and otherwise falls back to TCG software
emulation — reporting at runtime which it chose. Forcing `kvm` hard-fails with a clear
message when unavailable; `tcg` is always available.

The container is never run `--privileged`. The only device it may be granted is
`/dev/kvm` (with the unprivileged user added to the `kvm` group), and only as an opt-in
performance upgrade. TCG works rootless with zero device access, so the zero-privilege
path always works.

We record this because the obvious "fix" when KVM seems unavailable is to run as root or
add `--privileged`; that needlessly grants host access. The intended escalation path is
narrow — add exactly `/dev/kvm`, nothing more — and the default degrades safely to TCG.
