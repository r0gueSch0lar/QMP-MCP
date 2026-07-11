# Guests start active by default; `QMP_MCP_AUTO_START=false` opts into paused-for-inspection

`create_instance` now brings the Guest to **RUNNING** by default. `QMP_MCP_AUTO_START`
defaults to **true** (`config.rs` / `config.ts`), so the server issues the inline QMP `cont`
sub-second after launch and returns an executing Guest. Setting `QMP_MCP_AUTO_START=false`
restores the old behavior: the Guest loads **PAUSED**, frozen at the `-S` startup pause, and
runs only on an explicit `resume_instance`. This **reverses the "PAUSED by default" contract**
recorded in issues #8 and #10 — the re-opening those issues asked for before the global default
could be flipped.

The trigger is issue #26. Every Guest launches with `-S` (vCPUs frozen at prelaunch) so the
server can do its deterministic setup — arm the VNC Display password over QMP (ADR-0010), wire
event capture — before any guest code runs. Under the old paused default, the `cont` was sent
only later by a caller-paced `resume_instance`, so the wall-clock gap between launch and resume
was unbounded (agent/LLM/human think time). kvmclock/TSC-sensitive Guests — notably generic
cloud images like the Debian 12 generic cloud image — then see a multi-second clock jump on
`cont` and kernel-panic in early userspace ("Attempted to kill init! exitcode=0x00000200"). The
paused default made the *common* path a footgun; the confirmed fast path (`cont` within ~0.5s)
boots cleanly.

Crucially, **`-S` is retained** — this ADR flips only *whether the server auto-`cont`s*, not
*whether the Guest loads paused*. The load-paused-then-`cont` sequence and its setup window are
unchanged; by default the `cont` now fires inline (sub-second, matching #26's clean path)
instead of waiting for a separate tool call. The single `-S` emission site
(`rust/src/instance/hardware_spec.rs`, `typescript/src/instance/hardware-spec.ts`) and every
ADR-0012 golden fixture are untouched. The inspect-before-run capability that #8/#10 established
is **preserved as an opt-out**, not removed: `QMP_MCP_AUTO_START=false` gives back the exact
old semantics, still asserted by tests across both implementations.

We record this because it reverses a previously-recorded decision, and the reversal rests on two
load-bearing choices:

1. **Flip the default, not the mechanism.** The Guest still launches with `-S` and still passes
   through the deterministic setup window before executing; only the *default value* of
   `QMP_MCP_AUTO_START` changes (false → true). Removing `-S`, or making create skip the load-
   paused phase, would discard the inspect-before-run guarantee and the display/event setup that
   depend on it — so neither is done. The sole behavioral change an operator sees is that create
   returns RUNNING instead of PAUSED.
2. **Opt-out, not removal — the paused contract still exists, just off the default path.**
   `QMP_MCP_AUTO_START=false` reproduces the issue-#8/#10 behavior byte-for-byte (no `cont`, land
   PAUSED, resume later), and the test suites pin auto-start off to exercise that lifecycle
   deterministically (the real-QEMU integration test drives create→PAUSED→resume→RUNNING). The
   default is asserted separately in the config tests. Agents or deployments that relied on
   create→PAUSED must now pass `QMP_MCP_AUTO_START=false`; this is a breaking change to the
   default, justified because active-by-default matches the obvious expectation ("create a VM →
   it runs") and removes the #26 panic from the common path.
