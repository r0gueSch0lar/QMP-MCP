# Rust variant: a same-repo second implementation on rmcp + tokio

A Rust variant of qmp-mcp lives in a `rust/` subtree of this repo, as a **second
implementation of the same bounded context** — not a new context and not a fork of
the domain. It shares the root `CONTEXT.md` and ADRs 0001–0010 (the domain model is
language-agnostic) and targets full behavioral parity with the TypeScript
implementation, so the two can be cross-validated against the same ADRs and inputs
(same spec → same argv, same Command Policy verdicts, same fail-closed behavior).

**Framework.** Built on `rmcp` 0.16 (the official MCP Rust SDK) + tokio. This is
idiomatic-Rust and shaped unlike mcp-framework, so the deltas are deliberate:

- **Tools are compile-time, not directory-discovered.** All tools are `#[tool]`
  methods on a server struct inside a `#[tool_router]` impl, with `#[tool_handler]`
  implementing `ServerHandler`. Tool params are structs deriving `serde::Deserialize`
  + `schemars::JsonSchema`, taken as `Parameters<T>`. (mcp-framework auto-discovered
  one-file-per-tool from `dist/tools`; a future reader should not expect that here.)
- **State is shared, not module-global.** The single-instance Orchestrator is
  `Arc<Mutex<Orchestrator>>`, so concurrent tool calls serialize on the one Instance —
  which structurally precludes the create-time TOCTOU the TS build had to guard against.
- **Auth is middleware, not a provider.** rmcp ships no `APIKeyAuthProvider`.
  `StreamableHttpService` (feature `transport-streamable-http-server`) is a tower
  service, so the fail-closed HTTP auth (ADR-0005) is a tower/axum middleware layer in
  front of it. stdio (`transport-io`) stays auth-free. Transport is selected by
  `QMP_MCP_TRANSPORT`, as in the TS impl.

**QMP client is hand-rolled and dynamic.** A small client over a tokio `UnixStream`
with `serde_json`: newline-delimited framing, greeting → `qmp_capabilities` handshake,
id-correlated request/response with a per-command timeout, error mapping, and async
events into the Event Buffer. Everything is dynamic `{execute, arguments, id}` JSON —
matching the dynamic Command Policy (ADR-0003) and `qmp_execute`, and mirroring the TS
design — rather than the typed `qapi` crate, whose typed value does not pay off when
the policy gates dynamic command names.

Where a Rust-specific choice is itself an ADR-worthy trade-off, it gets its own ADR;
this one records the foundational stance.
