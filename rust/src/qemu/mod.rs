//! The QEMU integration boundary: the single injectable driver seam the
//! Orchestrator depends on (ADR-0011). A second implementation of the shared
//! bounded context, mirroring `../../typescript/src/qemu/*`.
//!
//! The seam itself is the [`driver::QemuDriver`] port with its in-memory
//! [`driver::FakeQemuDriver`] test double (slice #20). The production driver
//! [`real_driver::RealQemuDriver`] — a tokio child process wrapping the hand-rolled
//! dynamic [`qmp_client::QmpClient`] — drops in behind the same port (slice #21).

pub mod driver;
pub mod qmp_client;
pub mod real_driver;
