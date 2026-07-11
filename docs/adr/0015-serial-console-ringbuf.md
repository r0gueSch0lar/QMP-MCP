# Serial Port capture: two operator-selected backends (ringbuf / spool); read-only by default, board-agnostic through `-serial`

A Guest's **Serial Port** can be captured on demand (`serial: true` on the Hardware Spec). One
of two **operator-selected** backends (`QMP_MCP_SERIAL_BACKEND`, default `ringbuf`) carries the
output, and either way the argv is a single `-serial chardev:serialbuf` bound to a `-chardev`
the server configures — so `buildArgv` stays a pure, deterministic function of resolved config
(ADR-0002) and no board-specific UART device name is ever emitted.

**ringbuf (default).** `-chardev ringbuf,id=serialbuf,size=<N>` — a bounded ring buffer QEMU
maintains in-tree, drained over the existing QMP socket with `ringbuf-read`. No new host binary,
socket, file, or pty; `read_serial` is one more `execute()` on the QMP write-mutex that already
exists. This is the deciding factor over the alternatives: a *server-owned* `-serial
unix:…,server=on` backend drained into our own buffer would duplicate the `real_driver`
dial/drain/teardown machinery for a second always-on channel in both implementations (and is the
same Event-Buffer-style server-side buffer we chose not to build); and the `-nographic` stdio
mux is a dead end because the child's stdout is `/dev/null` and `-nodefaults` strips the default
serial port anyway. `ringbuf-read` **drains** (advances the read pointer → returns and clears
output produced *since the last read*, not re-readable) and the ring is **lossy on overflow**
(oldest bytes dropped if the Guest outpaces it) — hence the **1 MiB** default (`QMP_MCP_SERIAL_BUFFER_BYTES`).
QEMU requires a **power-of-two** size, so the server validates it fail-closed with an actionable
message before launch.

**spool.** `-chardev file,id=serialbuf,path=<resolved>` writing the serial output to a **host
file** under the operator's `QMP_MCP_SERIAL_SPOOL_DIR` root. This gives *persistent,
re-readable, unbounded* logs — the non-destructive property `ringbuf` lacks — with **zero
server-side retention machinery**: QEMU and the filesystem do the work. The file lives in a
**per-Instance subdirectory the spec may name** (`serialSpool`), validated with the **same name
allowlist as Image/ISO Store names** (no `/`, `..`, or absolute paths) and resolved *under* the
operator root — the Image Store's "by name, never by host path" boundary applied to serial
output, so the agent can never name a host path. The file is **truncated per boot** (QEMU
default; a future `QMP_MCP_SERIAL_SPOOL_APPEND` could opt into accumulate). The spool backend is
**output-only** (a file chardev cannot be written), so `write_serial` is unavailable under it.
`QMP_MCP_SERIAL_BACKEND=spool` without `QMP_MCP_SERIAL_SPOOL_DIR` is an operator misconfiguration
and fails closed at config load.

**`-serial` is the normalizer across boards.** It binds the chardev to *the machine's first
serial port*, and QEMU resolves which UART that is — 16550 ISA (`ttyS0`) on `q35`/`pc`, PL011
(`ttyAMA0`) on ARM `virt`, the `raspi*` PL011/mini-UART pair. The pure generator emits **no
board-specific device name**; the explicit `-serial chardev:` re-creates the port `-nodefaults`
stripped and, per the manpage's "unless redirected elsewhere explicitly," wins over the
`-nographic` console mux. The **guest side is not ours**: which `/dev/ttyXXX` the guest kernel
uses as its console is set by the guest's `console=` cmdline, which qmp-mcp cannot inject for an
arbitrary image (ADR-0007). The read-only **`get_serial`** tool surfaces that advisory — a
**best-effort per-machine console-device guess** (`ttyS0` for `q35`/`pc`/`microvm`/`isapc`,
`ttyAMA0` for `virt`, an honest "unknown" fallback since `machine` is a free string) plus the
`raspi*` two-UART caveat — exactly as `get_share` surfaces the 9p mount command it cannot run. It
also advertises the active backend and read semantics so an agent knows whether `read_serial`
drains or tails. `raspi*` port ordering is deferred (v1 binds the first port).

The trust split mirrors folder sharing (ADR-0014): **read-only by default, writable only by
operator opt-in.** *Reading* touches no host surface, so capture needs no gate — the **agent's
read lever is the boolean** `serial: true` (the chardev id is the fixed server constant
`serialbuf`; the only other agent input is the validated `serialSpool` subdir *name*). *Writing*
to the console is input injection, not a report, so it is **off by default** behind
`QMP_MCP_ALLOW_SERIAL_WRITE` (fail-closed, same shape as `QMP_MCP_ALLOW_SHARE_WRITE`). The
host-touching knobs — backend, spool root, buffer size, write-enable — are all **operator env**;
the agent never selects them. `serialSpool` that cannot apply (ringbuf backend, or `serial:
false`) is **ignored with a specific log warning** rather than rejected, so one spec stays
portable across ringbuf and spool operators.

`read_serial` (always registered) takes optional `maxBytes` (cap) + `format` (`utf8`|`base64`,
default utf8): ringbuf drains up to `maxBytes`; spool returns the non-destructive **tail**. It
errors when no Instance / no Serial Port. `write_serial` (also always registered) takes `data` +
`format` and writes **raw bytes, no auto-newline** (the agent submits its own `\n`); it errors
actionably when writing is disabled, the backend is spool, or nothing is running.

We record this because the mechanism choices constrain every later increment, and the write
asymmetry is safety-critical:

1. **Borrow QEMU's buffer (ringbuf) over building a server-side one; spool is the persistent
   alternative via the filesystem, not server memory.** The richer ephemeral ergonomics
   (non-destructive cursor reads, timestamps, a `wait_for_serial`) remain reachable later without
   an argv change (poll `ringbuf-read` into a retained buffer). But the common persistent want —
   "keep the log, re-read it, inspect after teardown" — is delivered *now* by the spool backend
   using QEMU + the filesystem, so we never build the Event-Buffer-style retention machinery.
2. **Read is unprivileged; write is operator-gated behind a single gate.** `read_serial` rides on
   `serial: true`. Its sibling `ringbuf-write` types into the guest console (keyboard-equivalent
   control), so the *only* sanctioned write path is the dedicated `write_serial` tool behind
   `QMP_MCP_ALLOW_SERIAL_WRITE`. `write_serial` is **always registered and errors when disabled**
   (matching the auto-discovery / `get_share` "always present, report state" pattern — hiding a
   tool is cosmetic; the env check is the real gate). To keep it a *single* gate, `ringbuf-write`
   is placed on the **immutable Command-Policy hard denylist** (ADR-0003): an operator can never
   re-enable it on the `qmp_execute` passthrough, so there is no second, differently-gated
   console-write path. Dedicated tools bypass the policy, so hard-denying `ringbuf-write` does not
   block `write_serial`. `ringbuf-read` is left off the default allowlist (the dedicated
   `read_serial` is the blessed path, exactly as `screendump` is kept off it).
3. **`-serial` keeps the argv generator board-agnostic and pure.** A normalized `-serial
   chardev:` rather than a per-board UART device preserves ADR-0002 purity and keeps the ADR-0012
   golden-fixture churn to one opt-in `-chardev`+`-serial` pair, identical across machines and
   byte-for-byte across the TypeScript and Rust generators.
4. **The operator owns every host-touching knob; the agent gets a boolean and a validated name.**
   Backend, spool root, buffer size, and write-enable are operator env. The agent's entire lever
   set is `serial: true` plus an optional `serialSpool` subdir *name* resolved under the operator
   root — the Image Store's "by name, never by host path" invariant applied to serial output,
   which is what keeps an opt-in serial feature from becoming an arbitrary host-file write.
