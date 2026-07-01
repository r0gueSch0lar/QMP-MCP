//! The QEMU integration boundary: the single injectable driver seam the
//! Orchestrator depends on (ADR-0011). A second implementation of the shared
//! bounded context, mirroring `../../src/qemu/*`.
//!
//! This slice contributes the seam itself — the [`driver::QemuDriver`] port and
//! its in-memory [`driver::FakeQemuDriver`] test double — plus a fail-closed
//! production placeholder. The real driver (a tokio child process wrapping a
//! hand-rolled dynamic QMP client) drops in behind the same port in slice #21.

pub mod driver;
