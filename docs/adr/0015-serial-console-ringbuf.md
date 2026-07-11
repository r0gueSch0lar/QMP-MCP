# Serial console via an in-tree QEMU ringbuf chardev; read-only, board-agnostic through `-serial`

A guest's serial console can be captured into a **ring buffer** QEMU maintains itself —
`-chardev ringbuf,id=serialbuf,size=<N>` bound to the machine's serial port with
`-serial chardev:serialbuf` — and drained over the existing QMP socket with the
`ringbuf-read` command. The `ringbuf` chardev is in-tree in `qemu-system-*`, so `buildArgv`
stays a pure, deterministic function of the Hardware Spec (ADR-0002) and no new host binary,
socket, file, or pty is introduced. This is the deciding factor over the alternatives: a
server-owned `-serial unix:…,server=on` backend that qmp-mcp drains into its own buffer would
duplicate the QMP dial/drain/teardown machinery (`real_driver`) for a second always-on
channel in both implementations; and the `-nographic` stdio mux is a dead end because the
child's stdout is `/dev/null` and `-nodefaults` strips the default serial port anyway.
Everything instead rides the QMP write-mutex that already exists — `read_serial` is one more
`execute()`.

**`-serial` is the normalizer across boards.** It binds the backend to *the machine's first
serial port*, and QEMU resolves which UART that is per board — 16550 ISA (`ttyS0`) on
`q35`/`pc`, PL011 (`ttyAMA0`) on ARM `virt`, and the `raspi*` boards' PL011/mini-UART pair.
So the pure argv generator emits **no board-specific device name** (`-device isa-serial` /
`pl011` never appear); one spec field produces the right device everywhere, and the explicit
`-serial chardev:` redirect re-creates the port that `-nodefaults` stripped and, per the
manpage's "unless redirected elsewhere explicitly", wins over the `-nographic` console mux.
The host side is fully normalizable; the **guest side is not ours** — which `/dev/ttyXXX` the
guest kernel treats as its console is set by the guest's `console=` cmdline, which qmp-mcp
cannot inject for an arbitrary image (ADR-0007). That advisory — the expected console device
per machine family, plus the `raspi*` two-UART ordering caveat (`-serial null -serial
chardev:serialbuf`) — is surfaced by a read-only `get_serial` tool, exactly as `get_share`
surfaces the 9p mount command it cannot run. `raspi*` port ordering is deferred: v1 binds the
first port (clean on `q35`/`pc`/`virt`) and documents the caveat.

The trust split mirrors folder sharing (ADR-0014): **read-only by default, writable only by
operator opt-in.** *Reading* is strictly weaker than a host share — it exposes only guest
console *output* and touches **no host surface** — so capture needs no gate: the **agent's read
lever is the boolean** `serial: true` on the Hardware Spec, which carries no path, device, or id
(the chardev id is the fixed server constant `serialbuf`, no free string, no injection).
*Writing* to the console is the privileged half and is **off by default**: the `write_serial`
tool that wraps `ringbuf-write` is registered only when the operator sets
`QMP_MCP_ALLOW_SERIAL_WRITE=true`, fail-closed in the exact shape of `QMP_MCP_ALLOW_SHARE_WRITE`
and `QMP_MCP_ALLOW_HOST_NET` (the boolean must be literally `"true"`/`"false"`). The agent can
never escalate to writable, because typing into the guest console is input injection, not a
report. Buffer size defaults to **1 MiB** and is operator-tunable via
`QMP_MCP_SERIAL_BUFFER_BYTES`; because the `ringbuf` chardev requires a **power-of-two** size
(QEMU aborts launch with "size of ringbuf chardev must be power of two"), the server validates
it fail-closed with an actionable message *before* launch rather than letting QEMU reject the
argv.

Two `ringbuf` semantics are load-bearing and easy to misread later. `ringbuf-read` **drains**:
it advances the read pointer, so a read returns the output produced *since the last read* and
cannot re-read it — `read_serial`'s contract is therefore "returns and clears new serial
output," polled like `get_events`, and the console cannot be consumed by both the tool and a
second reader. And the ring is **lossy on overflow**: if the guest outpaces the buffer between
reads the oldest bytes are silently dropped — hence the 1 MiB default (the QEMU default of
65536 holds only a partial verbose boot). `read_serial` defaults to `format:"utf8"` with
`base64` available for non-UTF8 early-boot bytes.

We record this because the mechanism choice constrains every later increment, and one
capability asymmetry is safety-critical:

1. **Borrow (native `ringbuf`) over build (a server-owned buffer).** The richer ergonomics —
   non-destructive cursor reads (`since_seq`, like `get_events`), line timestamps, a
   `wait_for_serial` regex tool mirroring `wait_for_event` — are a real future want, but they
   are reachable **without any argv change**: qmp-mcp can poll `ringbuf-read` on a timer into a
   retained in-process buffer (draining faster than the ring fills also removes the overflow
   loss). Building that machinery now, before a demonstrated need, is not justified when the
   same `-chardev ringbuf` argv supports graduating to it later.
2. **Read is unprivileged; write is operator-gated, off by default.** `read_serial` wraps
   `ringbuf-read` (output only) and rides on `serial: true`. Its sibling `ringbuf-write` lets
   the agent *type into the guest console* — a keystroke-injection capability equivalent to
   keyboard control, not a report — so the `write_serial` tool that wraps it is registered only
   under `QMP_MCP_ALLOW_SERIAL_WRITE=true`, the same fail-closed operator opt-in that governs the
   9p share's writability. On the `qmp_execute` passthrough, `ringbuf-write` remains
   independently governed by the Command Policy (ADR-0003), which gates command *names*. This is
   the same read-only-unless-the-operator-allows line ADR-0014 draws for the share.
3. **`-serial` keeps the argv generator board-agnostic and pure.** Emitting a normalized
   `-serial chardev:` rather than a per-board UART device is what preserves ADR-0002 purity and
   keeps the ADR-0012 golden-fixture churn to a single opt-in `-chardev`+`-serial` pair,
   identical across machines and byte-for-byte across the TypeScript and Rust generators.
