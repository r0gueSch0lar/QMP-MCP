//! The QEMU driver port — the single primary test seam of this server (ADR-0011).
//!
//! It abstracts the two things only a real machine can do: spawn a
//! `qemu-system-*` process with a given argv, and own the live QMP Session to it.
//! The [`crate::instance::orchestrator::Orchestrator`] depends on this trait (via a
//! `Box<dyn QemuDriver>`), so the whole lifecycle is exercisable against the
//! in-memory [`FakeQemuDriver`] with no real process or socket, while production
//! wires in the real driver.
//!
//! Mirrors `../../typescript/src/qemu/driver.ts` and `../../typescript/src/qemu/fake-driver.ts`
//! behaviorally. The seam is deliberately narrow: a single [`QemuDriver::launch`]
//! hands back a live [`InstanceHandle`], and everything else flows through that
//! handle (`execute` for QMP commands — including `query-status` and the
//! `stop`/`cont` pause controls — and `close` to terminate). The real driver
//! ([`crate::qemu::real_driver::RealQemuDriver`] — tokio child process + hand-rolled
//! QMP client) sits behind the same shape; this module defines the port and the
//! in-memory fake the lifecycle tests run against.

use async_trait::async_trait;
use tokio::sync::broadcast;

use super::qmp_client::QmpEvent;

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

/// A future that resolves when an Instance's qemu process / QMP Session has exited for
/// ANY reason (crash, guest-initiated poweroff, external kill, or an explicit close).
/// Boxed so the [`InstanceHandle`] trait stays object-safe. Mirrors the TS
/// `InstanceProcess.exited` promise.
pub type ExitedFuture = std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>;

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

    /// Subscribe to this Instance's async QMP events (the hook the Event Buffer feeds
    /// from, slice #24). Each returned receiver observes every event broadcast from
    /// the moment of subscription — the Orchestrator subscribes when the Instance is
    /// created, so the buffer spans the Instance's whole life.
    fn subscribe_events(&self) -> broadcast::Receiver<QmpEvent>;

    /// A future that resolves when this Instance's qemu process / QMP Session has
    /// exited — the hook the Orchestrator's exit-watch task awaits to reconcile a
    /// crashed / powered-off / externally-killed Instance back to NONE without an
    /// explicit `destroy_instance` (issue #28). Resolves immediately when the Session
    /// has already closed. The real driver wires this to the QMP socket close (which
    /// qemu triggers on exit); the fake driver fires it on `close` or on a
    /// test-driven simulated exit. Mirrors the TS `InstanceProcess.exited`.
    fn exited(&self) -> ExitedFuture;

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
#[cfg(test)]
use tokio::sync::watch;

/// Records every launch and hands back a [`FakeInstanceHandle`]. Spawns no process
/// and opens no socket; this is what makes the Orchestrator's lifecycle testable
/// end-to-end without a real QEMU. Tests can inspect [`launches`](FakeQemuDriver::launches)
/// to assert what the Orchestrator built and handed over (e.g. exactly one launch
/// under concurrent create attempts).
#[cfg(test)]
pub(crate) struct FakeQemuDriver {
    /// When set, [`launch`](QemuDriver::launch) fails with this message.
    launch_error: Option<String>,
    /// Every request that reached [`launch`](QemuDriver::launch), shared so a test
    /// can read it after the driver has moved into the Orchestrator.
    launches: Arc<Mutex<Vec<LaunchRequest>>>,
    /// The sender for the MOST-RECENTLY launched Instance's synthetic event stream.
    /// Each launch installs a FRESH channel here — so, exactly like the TS fake's
    /// per-process listeners, an event emitted on one Instance's sender never reaches a
    /// later Instance. A test reads this (via [`events_slot`](FakeQemuDriver::events_slot))
    /// after `create_instance` to emit synthetic events into the current Instance.
    last_events: Arc<Mutex<Option<broadcast::Sender<QmpEvent>>>>,
    /// The exit-signal sender for the MOST-RECENTLY launched Instance. Each launch
    /// installs a FRESH latch here, so a test can drive an unexpected qemu exit
    /// (crash/SIGKILL) on the current Instance via [`exit_slot`](FakeQemuDriver::exit_slot)
    /// — the fake equivalent of the TS fake process's `simulateExit` (issue #28).
    last_exit: Arc<Mutex<Option<Arc<watch::Sender<bool>>>>>,
    /// The QMP command names the MOST-RECENTLY launched Instance's handle has been
    /// asked to `execute`, in order. Cleared on each launch and shared with the
    /// handle, so a test can assert what the Orchestrator issued (the equivalent of
    /// the TS fake process's `executed`) — e.g. that create auto-starts with `cont`.
    last_commands: Arc<Mutex<Vec<String>>>,
}

#[cfg(test)]
impl Default for FakeQemuDriver {
    fn default() -> Self {
        Self {
            launch_error: None,
            launches: Arc::new(Mutex::new(Vec::new())),
            last_events: Arc::new(Mutex::new(None)),
            last_exit: Arc::new(Mutex::new(None)),
            last_commands: Arc::new(Mutex::new(Vec::new())),
        }
    }
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

    /// A handle onto the current Instance's event sender, readable after a launch so a
    /// test can emit synthetic QMP events into the running Instance's stream — the
    /// equivalent of the TS fake driver's `emitEvent`. Each launch replaces the sender
    /// with a fresh one, so events cannot bleed across Instances.
    pub(crate) fn events_slot(&self) -> Arc<Mutex<Option<broadcast::Sender<QmpEvent>>>> {
        Arc::clone(&self.last_events)
    }

    /// A handle onto the current Instance's exit-signal sender, readable after a launch
    /// so a test can drive an unexpected qemu exit (crash/SIGKILL) on the running
    /// Instance — the equivalent of the TS fake process's `simulateExit`. Sending `true`
    /// resolves the handle's [`exited`](InstanceHandle::exited) future WITHOUT going
    /// through `close`, letting a test exercise the Orchestrator's exit-reconciliation
    /// path (issue #28). Each launch replaces the latch with a fresh one.
    pub(crate) fn exit_slot(&self) -> Arc<Mutex<Option<Arc<watch::Sender<bool>>>>> {
        Arc::clone(&self.last_exit)
    }

    /// A handle onto the current Instance's executed-command log, readable after a
    /// launch so a test can assert which QMP commands the Orchestrator issued (e.g.
    /// that create auto-started with `cont`). Each launch clears it.
    pub(crate) fn commands(&self) -> Arc<Mutex<Vec<String>>> {
        Arc::clone(&self.last_commands)
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
        // A fresh per-Instance event channel (capacity matches the QMP client's): the
        // handle owns the sender, the Orchestrator subscribes a receiver, and the test
        // emits via the recorded sender clone.
        let (events_tx, _rx) = broadcast::channel(256);
        *self.last_events.lock().expect("fake events slot mutex") = Some(events_tx.clone());
        // A fresh per-Instance exit latch: the handle fires it on close (or a test fires
        // it via the recorded slot to simulate an unexpected qemu exit).
        let exit_tx = Arc::new(watch::channel(false).0);
        *self.last_exit.lock().expect("fake exit slot mutex") = Some(Arc::clone(&exit_tx));
        // Fresh per-Instance command log: clear it and share it with the handle so a
        // test reads only the current Instance's executed QMP commands.
        self.last_commands
            .lock()
            .expect("fake commands mutex")
            .clear();
        let commands = Arc::clone(&self.last_commands);
        Ok(Box::new(FakeInstanceHandle::new(
            events_tx, exit_tx, commands,
        )))
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
    /// The shared synthetic event stream this handle exposes via `subscribe_events`.
    events_tx: broadcast::Sender<QmpEvent>,
    /// Latches `true` when the Instance "exits" — either an explicit `close` or a
    /// test-driven simulated unexpected exit (crash/SIGKILL) via the driver's exit slot.
    /// `exited` resolves once it is set, mirroring the real QMP-session close (issue #28).
    exit_tx: Arc<watch::Sender<bool>>,
    /// Shared log of QMP command names this handle has executed (for test assertions).
    commands: Arc<Mutex<Vec<String>>>,
}

#[cfg(test)]
impl FakeInstanceHandle {
    fn new(
        events_tx: broadcast::Sender<QmpEvent>,
        exit_tx: Arc<watch::Sender<bool>>,
        commands: Arc<Mutex<Vec<String>>>,
    ) -> Self {
        Self {
            // Starts NOT running, modelling QEMU's `-S` startup pause: the Guest is
            // loaded but frozen until the first `cont` (create's auto-start by default,
            // or resume_instance). So `query-status` reads `paused` until that `cont`;
            // with auto-start off the Instance stays PAUSED (#10; ADR-0016).
            state: Mutex::new(FakeHandleState {
                running: false,
                closed: false,
            }),
            events_tx,
            exit_tx,
            commands,
        }
    }
}

#[cfg(test)]
#[async_trait]
impl InstanceHandle for FakeInstanceHandle {
    async fn execute(
        &self,
        command: &str,
        args: Option<serde_json::Value>,
    ) -> Result<serde_json::Value, DriverError> {
        // Record the command for test assertions (e.g. create auto-start issues cont).
        self.commands
            .lock()
            .expect("fake commands mutex")
            .push(command.to_string());
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
            // In-place control commands that leave the run-state unchanged; QEMU
            // answers with the empty success object. `set_password` arms the vnc
            // Display's password over QMP (ADR-0010) — the Orchestrator issues it
            // before starting the noVNC Viewer.
            "system_reset" | "system_powerdown" | "set_password" => Ok(serde_json::json!({})),
            // Answered dynamically from the run-state, so `get_status` reflects a
            // pause without the test wiring it up.
            "query-status" => Ok(serde_json::json!({
                "status": if state.running { "running" } else { "paused" },
                "running": state.running,
            })),
            // A minimal, well-shaped stand-in for the read-only queries the curated
            // list_block_devices / query_cpus tools and `qmp_execute` forward.
            "query-block" => Ok(serde_json::json!([{ "device": "virtio0", "removable": false }])),
            "query-cpus-fast" => Ok(serde_json::json!([{ "cpu-index": 0, "target": "x86_64" }])),
            "query-version" => Ok(serde_json::json!({ "qemu": { "major": 9, "minor": 0 } })),
            // `screendump` writes an arbitrary host file at the server-chosen path in
            // its `filename` argument; the fake writes a tiny PNG-ish blob there so the
            // Orchestrator can read it back, base64-encode it, and delete it.
            "screendump" => {
                let filename = args
                    .as_ref()
                    .and_then(|a| a.get("filename"))
                    .and_then(|f| f.as_str())
                    .ok_or_else(|| {
                        DriverError("screendump requires a filename argument.".to_string())
                    })?;
                std::fs::write(filename, b"\x89PNG\r\n\x1a\nFAKE").map_err(|e| {
                    DriverError(format!("fake screendump failed to write {filename}: {e}"))
                })?;
                Ok(serde_json::json!({}))
            }
            other => Err(DriverError(format!(
                "FakeInstanceHandle has no canned response for QMP command \"{other}\"."
            ))),
        }
    }

    fn subscribe_events(&self) -> broadcast::Receiver<QmpEvent> {
        self.events_tx.subscribe()
    }

    fn exited(&self) -> ExitedFuture {
        let mut rx = self.exit_tx.subscribe();
        Box::pin(async move {
            // `wait_for` checks the current value first, so an already-exited handle
            // resolves at once; an error (sender dropped) also means it is gone.
            let _ = rx.wait_for(|&exited| exited).await;
        })
    }

    async fn close(&self) -> Result<(), DriverError> {
        self.state.lock().expect("fake handle mutex").closed = true;
        // An explicit close also fires the exit latch (mirrors the TS fake's `close`,
        // which resolves `exited`); the Orchestrator's watcher is guarded so this is a
        // no-op reconcile.
        let _ = self.exit_tx.send(true);
        Ok(())
    }
}
