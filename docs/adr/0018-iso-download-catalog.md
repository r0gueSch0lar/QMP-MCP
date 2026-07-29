# ISO download catalog: fetch OS images into the ISO Store on demand, from an operator-curated, overridable list, behind a fail-closed gate

The server can **download OS installation images into the ISO Store on demand** (`list_iso_catalog`
/ `download_iso` / `get_download`). An agent picks an image by a short `id` from a **catalog** — a
JSON list of distros, each carrying one or more mirror URLs — and the server streams it into the
ISO Store, where `list_isos` and a `cdrom` spec can then use it by name. The feature is an
**operator opt-in** (`QMP_MCP_ALLOW_DOWNLOAD`), the catalog is **operator-overridable**
(`QMP_MCP_ISO_CATALOG`), and downloads run in the **background** with pollable progress. This is
recorded because it adds outbound network egress, relaxes the ISO Store's read-only stance, and
constrains both implementations and the shipped images.

**The agent names an image, never a URL — the operator owns the catalog.** The whole trust model
turns on this: `download_iso` takes a catalog `id`, and the catalog is operator-controlled data
(the built-in list, or a file the operator points `QMP_MCP_ISO_CATALOG` at). The agent can only
fetch what the operator already blessed, from mirrors the operator already vetted — it can never
direct the server to an arbitrary host. This mirrors the Store allowlist philosophy (ADR-0006): the
agent references entries by name within an operator-defined namespace, never by host path or URL.
The catalog is **DATA, not code** — the built-in list ships as `assets/iso-catalog.json` (the Rust
crate embeds its copy via `include_str!`, the same posture as the embedded noVNC assets, ADR-0010;
the TypeScript runtime reads its copy at startup) and is kept **byte-identical across both variants
by a parity test** (ADR-0012), so the two servers always offer the same downloads. A validated
catalog is required at load: every entry's `filename` (the bare name the image is saved as) must
pass the **shared Store name allowlist** (ADR-0006, `store-path`), so a catalog can never name a
file that escapes the Store, and every mirror must be an `http(s)` URL — a malformed catalog fails
closed at startup (or, for a bad `QMP_MCP_ISO_CATALOG`, at first use in the TypeScript variant),
never at download time.

**Downloading is fail-closed behind `QMP_MCP_ALLOW_DOWNLOAD`.** Fetching an image is outbound
network egress that writes files, so — like `QMP_MCP_ALLOW_SERIAL_WRITE` (ADR-0015) and
`QMP_MCP_ALLOW_HOST_NET` (ADR-0009) — it is a capability the operator grants explicitly and the
agent can never grant itself: with the flag unset, `download_iso` returns an actionable "disabled"
error and never touches the network. The read-only surface (`list_iso_catalog`, `get_download`)
works regardless, so an agent can always *discover* what is available and *why* a download is
refused. There is deliberately **no size cap env var** in this slice: the stream is written
straight to disk (bounded memory), and a stall timeout bounds a wedged connection; a maximum-size
knob can be added later without changing the tool surface.

**The ISO Store becomes append-only, not read-write — a scoped relaxation of ADR-0006.** ADR-0006
defined the ISO Store as strictly read-only ("install media can never be modified") so a running
Guest could never mutate its own boot media. Downloading adds new media to that Store, which this
ADR permits **as an append: a download never overwrites an existing Store file** (an id whose
`filename` already exists is refused), so previously-present install media is still immutable — the
property ADR-0006 actually protects. The image streams to a `<name>.part` temp file in the Store
and is **atomically renamed** onto the final name only on success, so a partial or failed download
never appears as a usable ISO, and `list_isos` never surfaces a half-written file as bootable
media.

**One download at a time, in the background, with pollable progress — mirroring the single-Instance
discipline.** `download_iso` claims a **single download slot** (the same "one managed thing" shape
as the single-Instance Orchestrator, ADR-0001), starts the fetch on a background task, and returns
immediately with the initial status; a second concurrent request is refused while one is active.
This keeps the tool call fast — an OS image is often gigabytes and would blow past an MCP client's
per-call timeout if the tool blocked until done — and `get_download` reports live progress (state,
bytes/total/percent, the mirror being tried, the final path on completion). The double-start guard
is race-safe: the slot's check-and-set has no await between them on the single-threaded event loop
(TypeScript) / is taken under the manager lock (Rust). **Mirrors are tried in order**, the first
that succeeds wins, and a **per-chunk stall timeout** abandons a mirror that stops sending so a
dead connection can never wedge the slot. The network transport is the one injected seam — a
`Fetcher` (real `reqwest`/`fetch`, or a fake in tests) — so the manager's whole state machine
(gate, unknown id, existing file, failover, atomic rename, progress) is unit-tested against a fake,
exactly as the Orchestrator is tested against a fake QEMU driver (ADR-0011). The real fetcher
**follows redirects** (distro mirrors redirect heavily) and streams the body to disk with
backpressure.

**Configuration and the shipped images.** `QMP_MCP_ALLOW_DOWNLOAD` (default false) is the gate;
`QMP_MCP_ISO_CATALOG` (unset ⇒ the built-in list) points at a replacement catalog JSON and must be
an absolute path. Downloads land in the existing `QMP_MCP_ISO_DIR`. Both container images bundle
the built-in catalog at `/usr/local/share/qmp-mcp/iso-catalog.json` (a convenience path an operator
can inspect or point the variable at); the Rust binary additionally embeds it so bare-metal builds
need no runtime file. The Rust variant gains a `reqwest` dependency (pure-Rust `rustls-tls`, no
OpenSSL) for the HTTP client; the TypeScript variant uses the runtime's global `fetch`. **The
bundled URLs are best-effort** starting points captured at authoring time and *will* go stale as
distros publish new releases (rolling distros especially), and a few entries (e.g. RHEL) require
authentication a public URL cannot satisfy — which is exactly why `QMP_MCP_ISO_CATALOG` exists: the
mechanism is the deliverable, the bundled list is a maintainable-by-override template.
