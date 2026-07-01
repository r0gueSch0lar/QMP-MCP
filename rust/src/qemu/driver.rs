//! The QEMU driver port — the single primary test seam of this server (ADR-0011).
//!
//! It abstracts the two things only a real machine can do: spawn a
//! `qemu-system-*` process with a given argv, and own the live QMP Session to it.
//! The [`crate::instance::orchestrator::Orchestrator`] depends on this trait (via a
//! `Box<dyn QemuDriver>`), so the whole lifecycle is exercisable against the
//! in-memory [`FakeQemuDriver`] with no real process or socket, while production
//! wires in the real driver.
//!
//! Mirrors `../../src/qemu/driver.ts` and `../../src/qemu/fake-driver.ts`
//! behaviorally. The seam is deliberately narrow: a single [`QemuDriver::launch`]
//! hands back a live [`InstanceHandle`], and everything else flows through that
//! handle (`execute` for QMP commands — including `query-status` and the
//! `stop`/`cont` pause controls — and `close` to terminate). The real driver
//! ([`crate::qemu::real_driver::RealQemuDriver`] — tokio child process + hand-rolled
//! QMP client) sits behind the same shape; this module defines the port and the
//! in-memory fake the lifecycle tests run against.

use async_trait::async_trait;

/// Everything the driver needs to launch and connect to one Instance. Built by the
/// Orchestrator from a validated Hardware Spec (`binary` + generated `argv`,
/// already including `-qmp`) and the server-managed socket path.
#[derive(Debug, Clone)]
pub struct LaunchRequest {
    /// The `qemu-system-*` binary to exec (e.g. `qemu-system-x86_64`).
    pub binary: String,
    /// The full argv (excluding the program name), already including `-qmp`.
    pub argv: Vec<String>,
    /// Path of the QMP UNIX socket the launched process will create.
    pub qmp_socket_path: String,
}

/// Raised by the driver when a launch fails or a QMP command cannot be completed.
/// The message is actionable and is surfaced (wrapped) to the agent by the
/// Orchestrator. Mirrors the error text the TS driver throws.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct DriverError(pub String);

/// A launched Instance with an established QMP Session. The handle owns the QMP
/// channel: callers drive the Guest exclusively through [`execute`](InstanceHandle::execute)
/// and tear everything down with [`close`](InstanceHandle::close).
///
/// `execute` takes `&self` (not `&mut self`) so the real driver can serve
/// id-correlated commands over the one shared socket with internal synchronisation,
/// exactly as the hand-rolled QMP client will in slice #21.
#[async_trait]
pub trait InstanceHandle: Send + Sync {
    /// Execute a QMP command against the Session, resolving with its `return`
    /// value (an opaque JSON value — the dynamic shape the Command Policy and
    /// `qmp_execute` rely on). `args` is the QMP `arguments` object, if any.
    async fn execute(
        &self,
        command: &str,
        args: Option<serde_json::Value>,
    ) -> Result<serde_json::Value, DriverError>;

    /// Terminate the process and close the QMP Session. Idempotent.
    async fn close(&self) -> Result<(), DriverError>;
}

/// The driver port. A single method launches an Instance and hands back a live
/// [`InstanceHandle`]; everything else flows through that handle. Object-safe (used
/// as `Box<dyn QemuDriver>`), `Send + Sync` so it can live inside the shared
/// `Arc<Mutex<Orchestrator>>`.
#[async_trait]
pub trait QemuDriver: Send + Sync {
    /// Spawn `binary` with `argv` and negotiate the QMP Session on
    /// `qmp_socket_path`, returning a handle that owns the running Instance.
    async fn launch(&self, request: LaunchRequest) -> Result<Box<dyn InstanceHandle>, DriverError>;
}

// ---------------------------------------------------------------------------
// The in-memory test double (used by the lifecycle + wiring tests). Gated to test
// builds so it never ships in the binary — the equivalent of `fake-driver.ts`.
// ---------------------------------------------------------------------------

#[cfg(test)]
use std::sync::{Arc, Mutex};

/// Records every launch and hands back a [`FakeInstanceHandle`]. Spawns no process
/// and opens no socket; this is what makes the Orchestrator's lifecycle testable
/// end-to-end without a real QEMU. Tests can inspect [`launches`](FakeQemuDriver::launches)
/// to assert what the Orchestrator built and handed over (e.g. exactly one launch
/// under concurrent create attempts).
#[cfg(test)]
#[derive(Default)]
pub(crate) struct FakeQemuDriver {
    /// When set, [`launch`](QemuDriver::launch) fails with this message.
    launch_error: Option<String>,
    /// Every request that reached [`launch`](QemuDriver::launch), shared so a test
    /// can read it after the driver has moved into the Orchestrator.
    launches: Arc<Mutex<Vec<LaunchRequest>>>,
}

#[cfg(test)]
impl FakeQemuDriver {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// A driver whose every `launch` fails, to exercise the Orchestrator's
    /// launch-failure recovery (slot released back to NONE, actionable error).
    pub(crate) fn with_launch_error(message: &str) -> Self {
        Self {
            launch_error: Some(message.to_string()),
            ..Self::default()
        }
    }

    /// A handle onto the recorded launches (clone of the shared log), so a test can
    /// assert the count and the argv the Orchestrator handed over even after the
    /// driver has moved into the Orchestrator.
    pub(crate) fn launches(&self) -> Arc<Mutex<Vec<LaunchRequest>>> {
        Arc::clone(&self.launches)
    }
}

#[cfg(test)]
#[async_trait]
impl QemuDriver for FakeQemuDriver {
    async fn launch(&self, request: LaunchRequest) -> Result<Box<dyn InstanceHandle>, DriverError> {
        if let Some(message) = &self.launch_error {
            return Err(DriverError(message.clone()));
        }
        self.launches
            .lock()
            .expect("fake launches mutex")
            .push(request);
        Ok(Box::new(FakeInstanceHandle::new()))
    }
}

/// Mutable inner state of a fake handle: a simulated Guest-CPU run-state that `stop`
/// pauses and `cont` resumes, so a later `query-status` reflects the pause/resume
/// the Orchestrator just performed (mirrors real QEMU), plus a closed flag that
/// makes `close` idempotent and post-close `execute` fail.
#[cfg(test)]
struct FakeHandleState {
    running: bool,
    closed: bool,
}

/// An in-memory [`InstanceHandle`] that answers QMP commands from a tiny table.
/// Interior mutability (a `std::sync::Mutex`) keeps `execute`/`close` on `&self`,
/// matching the real driver's shape.
#[cfg(test)]
struct FakeInstanceHandle {
    state: Mutex<FakeHandleState>,
}

#[cfg(test)]
impl FakeInstanceHandle {
    fn new() -> Self {
        Self {
            state: Mutex::new(FakeHandleState {
                running: true,
                closed: false,
            }),
        }
    }
}

#[cfg(test)]
#[async_trait]
impl InstanceHandle for FakeInstanceHandle {
    async fn execute(
        &self,
        command: &str,
        _args: Option<serde_json::Value>,
    ) -> Result<serde_json::Value, DriverError> {
        // No `.await` is held across this guard, so it never blocks the runtime.
        let mut state = self.state.lock().expect("fake handle mutex");
        if state.closed {
            return Err(DriverError("Instance is closed.".to_string()));
        }
        match command {
            // The pause/resume power commands flip the simulated run-state and return
            // QMP's empty success `{}` — a test need only assert the resulting
            // `query-status`.
            "stop" => {
                state.running = false;
                Ok(serde_json::json!({}))
            }
            "cont" => {
                state.running = true;
                Ok(serde_json::json!({}))
            }
            // Answered dynamically from the run-state, so `get_status` reflects a
            // pause without the test wiring it up.
            "query-status" => Ok(serde_json::json!({
                "status": if state.running { "running" } else { "paused" },
                "running": state.running,
            })),
            other => Err(DriverError(format!(
                "FakeInstanceHandle has no canned response for QMP command \"{other}\"."
            ))),
        }
    }

    async fn close(&self) -> Result<(), DriverError> {
        self.state.lock().expect("fake handle mutex").closed = true;
        Ok(())
    }
}
