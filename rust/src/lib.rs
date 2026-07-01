//! The qmp-mcp Rust variant library crate (ADR-0011).
//!
//! A second implementation of the shared qmp-mcp bounded context (see the root
//! `CONTEXT.md`), targeting full behavioral parity with the TypeScript server in
//! `../../src`. The binary (`src/main.rs`) is a thin entrypoint over this crate;
//! keeping the modules in a library lets the integration tests in `tests/` — and
//! the shared golden fixtures (ADR-0012) — exercise the pure functions directly.

pub mod config;
pub mod instance;
pub mod logging;
pub mod server;
