# Cross-implementation parity via shared golden fixtures

The TypeScript and Rust implementations (ADR-0011) must stay behaviorally
identical where the domain is deterministic: a given Hardware Spec must produce the
same QEMU argv, and a given QMP command must get the same Command Policy verdict, on
both sides. We make that a **test**, not a hope.

**Decision.** Language-neutral golden fixtures live at the repo root in `testdata/`
and are loaded by **both** implementations' test suites:

- `testdata/argv/*.json` — `{ spec, expectedArgv }`: a Hardware Spec and the exact
  argv the server must generate for it.
- `testdata/policy/*.json` — `{ command, arguments?, config?, expectedVerdict }`: a
  QMP command (and optional policy config) and whether the Command Policy allows or
  denies it, with the reason.

Each implementation has a thin loader that reads these files and asserts its own pure
functions (argv generator, Command Policy) match. Drift on either side fails the
fixture on whichever implementation changed.

**Why.** "Full parity" (ADR-0011) is otherwise unverifiable — two independently
mirrored test suites can silently diverge, and a reader can't tell which behavior is
canonical. A single shared corpus makes the contract explicit and executable, and
doubles as living documentation of spec→argv and command→verdict behavior.

**Trade-off.** The fixtures pin argv **ordering and exact flag spelling**, so an
intentional argv change means editing the shared corpus (and thereby consciously
updating both implementations' contract) — friction we accept, because an *unnoticed*
argv change across the security-sensitive generators is exactly what we want to catch.
Non-deterministic argv fragments (temp paths, sockets, allocated ports) are
placeholder-substituted in fixtures so they stay stable.

The alternative — each implementation mirrors the other's tests informally — was
rejected: it has no single source of truth and permits silent divergence.
