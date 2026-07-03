# qmp-mcp — Rust variant plan

A Rust implementation of qmp-mcp living in this `rust/` subtree, as a second
implementation of the one shared bounded context (see root `CONTEXT.md`). It targets
**full behavioral parity** with the TypeScript server in `../typescript/src`, built incrementally,
each slice cross-validated against the same domain ADRs and — for the deterministic
parts — the shared golden fixtures in `../testdata`.

- **Domain**: root `CONTEXT.md` (reused verbatim — no new terms).
- **Foundational decisions**: ADR-0011 (rmcp + tokio, `Arc<Mutex>` orchestrator, tower
  auth middleware, hand-rolled dynamic QMP client), ADR-0012 (shared parity fixtures).
  All of ADR-0001…0010 apply unchanged — they are language-agnostic.
- **Reference implementation**: `../typescript/src` (TypeScript, ~317 tests, all 17 tools).

## Tech stance (from the grilling)

- `rmcp` 0.16 (official MCP Rust SDK) + tokio. Tools are `#[tool]` methods on one
  server struct in a `#[tool_router]` impl; `#[tool_handler]` implements `ServerHandler`.
  Tool params are structs deriving `serde::Deserialize` + `schemars::JsonSchema`.
- Single-instance Orchestrator as `Arc<Mutex<Orchestrator>>` — concurrent tool calls
  serialize on the one Instance, structurally precluding the create-time TOCTOU.
- Transports: `stdio` (feature `transport-io`) + `StreamableHttpService` (feature
  `transport-streamable-http-server`), selected by `QMP_MCP_TRANSPORT`. Fail-closed
  HTTP auth is a **tower middleware** in front of the HTTP service (rmcp has no provider).
- Security modelled with **enums + newtypes + explicit validation functions** — no
  validation-DSL crate. serde/schemars cover shape; the security rules are hand-rolled,
  unit-tested, and a validated value gets a newtype so it can't reach argv generation
  unvalidated.
- Hand-rolled dynamic QMP client: `serde_json` over a tokio `UnixStream`,
  newline-framed, greeting → `qmp_capabilities`, id-correlated request/response with a
  per-command timeout, error mapping, async events into the Event Buffer.
- The one injectable seam is an `async_trait` `QemuDriver` trait (real + fake). Unit
  tests use `#[tokio::test]` + `FakeQemuDriver`; the single real-qemu integration test
  **runtime-skips** when a `qemu-system-*` binary is absent.
- Viewer: axum + its websocket + a tokio VNC relay, **noVNC assets embedded**
  (`include_dir!`) — bare-metal parity, traversal-safe by construction; same fail-closed
  Basic-auth + hardening as ADR-0010.
- Env config keeps the `QMP_MCP_` prefix, identical to the TS server.

## Config surface (parity with TS)

`QMP_MCP_TRANSPORT`, `QMP_MCP_HTTP_HOST`/`_PORT`, `QMP_MCP_API_KEY`,
`QMP_MCP_QEMU_BINARY`, `QMP_MCP_IMAGE_DIR`, `QMP_MCP_ISO_DIR`, `QMP_MCP_ALLOW_RAW_ARGS`,
`QMP_MCP_POLICY_*` (allow/deny overrides + file), resource caps, `QMP_MCP_VIEWER_*`
(enable/host/port/password), `QMP_MCP_LOG_LEVEL`. Mirror `../.env.example` and
`../policy.example.yaml`.

## Build slices (each → one issue, in order)

Each slice is a shippable vertical: it compiles, `cargo clippy -D warnings` and
`cargo fmt --check` pass, and it lands with its own tests.

1. **Crate scaffold + config + stdio + one tool.** `rust/Cargo.toml` (rmcp features
   `transport-io`; tokio, serde, schemars, tracing, thiserror). `config` module parsing
   `QMP_MCP_*` with actionable errors. `tracing` logger. Skeleton `QmpMcpServer` struct
   with `#[tool_router]`/`#[tool_handler]`, a trivial `get_status` tool, stdio transport
   wired end-to-end. Proves the rmcp wiring. **ADR-0011.**
2. **Hardware Spec + validation + argv generator.** serde/schemars param structs with
   enums (interface/mode/format/accel/display) + newtypes; explicit validators (charset
   allowlists, comma-escaping for `-drive`/`-machine`, caps, `extraArgs` gating); the
   pure argv generator. Wired to `../testdata/argv/*.json` via a fixture loader.
   **ADR-0002, 0008, 0009, 0012.**
3. **`QemuDriver` seam + orchestrator lifecycle.** `async_trait` `QemuDriver` (real
   deferred) + `FakeQemuDriver`; `Arc<Mutex<Orchestrator>>` with create/destroy/get/
   status, reject-on-running, instance = server lifetime. `create_instance`,
   `destroy_instance`, `get_instance`, `get_status` tools. `#[tokio::test]` via the fake.
   **ADR-0001, 0004.**
4. **QMP client + real driver.** Hand-rolled dynamic client (tokio `UnixStream`,
   newline framing, capabilities handshake, id correlation, timeout, error mapping,
   events). `RealQemuDriver` (tokio child process + client). The one real-qemu TCG
   integration test, runtime-skipped when qemu is absent. **ADR-0003, 0011.**
5. **Command Policy + qmp_execute + curated tools.** Immutable hard denylist + default
   allowlist + env/file overrides; `qmp_execute` forwarding arbitrary `{command,
   arguments}`; curated tools (pause/resume/reset/powerdown, list_block_devices,
   query_cpus, screendump-with-guard). Wired to `../testdata/policy/*.json`.
   **ADR-0003, 0012.** Note: policy gates command *names*; audit curated tools that pass
   dangerous *arguments* (e.g. screendump host-file write).
6. **Image Store + ISO Store.** rw Image Store + ro ISO Store, realpath containment via
   newtypes; `create_image`, `list_images`, `list_isos`. **ADR-0006.**
7. **Event Buffer + event tools.** Cursor ring buffer fed by the QMP client; `get_events`,
   `wait_for_event`; optional push as secondary. **ADR (events, pull-based primary).**
8. **HTTP transport + fail-closed auth.** `StreamableHttpService` behind a tower auth
   middleware; fail-closed API-key default with actionable errors. **ADR-0005, 0011.**
9. **Viewer.** axum + websocket + tokio VNC relay, embedded noVNC (`include_dir!`),
   fail-closed `QMP_MCP_VIEWER_PASSWORD` Basic auth, anti-framing + origin check +
   connection cap + backpressure. **ADR-0010.**
10. **Packaging + CI + docs.** `rust/Dockerfile` (cargo-chef multi-stage → slim runtime
    + qemu, non-root, `EXPOSE 8080 6080`). `.gitlab-ci.yml` Rust lane (fmt/clippy/test +
    dind image build) scoped with `rules:changes` on `rust/**`, reusing the `auto-merge`
    workflow. Example env + policy yaml; `rust/README.md`. Crate/binary `qmp-mcp`; Rust
    image tagged distinctly (`-rs`/`:rust`). **ADR-0011.**

## Parity gate

The `../testdata` golden fixtures (`argv/`, `policy/`) are authored once and asserted by
**both** implementations. Any intentional argv or policy change edits the shared corpus,
consciously updating the contract on both sides; any *unintentional* drift fails the
fixture. This is the executable definition of "full parity" (ADR-0012).
