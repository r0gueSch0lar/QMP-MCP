# Instance lifetime equals server lifetime; no cross-restart persistence

The single Instance is an in-memory singleton that lives and dies with the server
process. Requesting a new Instance while one exists is **rejected** (the agent must
explicitly destroy the current Instance first), not auto-replaced. On clean shutdown
the server terminates its `qemu-system-*` child. The server does **not** persist
Instance state, and on startup it does **not** adopt a QEMU left running by a previous
run — if its managed QMP socket / pidfile path is already occupied, it refuses to
start rather than clobbering or silently adopting.

We chose this because a cross-restart adoption protocol is fragile (re-deriving the
Hardware Spec and QMP state of a process we did not start), and silent auto-replace
makes an irreversible VM teardown implicit. Reject-and-refuse keeps the model honest;
persistence and adoption can be added later if a real need appears. A future reader
who expects the server to reconnect to a still-running VM after a restart should know
this was a deliberate omission, not an oversight.
