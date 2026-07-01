//! The single-instance lifecycle Orchestrator (ADR-0001/0004). It holds the one
//! managed Instance and drives it through its lifecycle:
//!
//!   NONE → STARTING → RUNNING ⇄ PAUSED → STOPPED → NONE
//!
//! A second implementation of the shared bounded context, mirroring
//! `../../src/instance/orchestrator.ts`: same state names, same reject-while-running
//! wording, same instance-lifetime = server-lifetime teardown.
//!
//! The Orchestrator depends on the [`QemuDriver`] port by injection (it holds a
//! `Box<dyn QemuDriver>`), so its whole lifecycle is testable against the fake
//! driver with no real QEMU. Crucially (ADR-0011), the Orchestrator lives behind an
//! `Arc<Mutex<Orchestrator>>`: because a caller holds the async mutex for the whole
//! duration of `create_instance` — including the `await` on `driver.launch` — two
//! concurrent create attempts cannot interleave. The second one only runs once the
//! first has fully committed to RUNNING, so it observes the occupied slot and is
//! rejected. This makes the create-time TOCTOU *structurally impossible*, so the
//! launch-token bookkeeping the single-threaded-async TypeScript port needs is not
//! required here.

use crate::config::PortRange;

use super::hardware_spec::{
    build_argv, parse_hardware_spec, resolve_accel, Accel, AccelResolution, ArgvOptions,
    HardwareSpec,
};
use crate::qemu::driver::{InstanceHandle, LaunchRequest, QemuDriver};

/// The lifecycle states an Instance moves through. `PAUSED` is entered by
/// [`Orchestrator::pause_instance`] (QMP `stop`) and left by
/// [`Orchestrator::resume_instance`] (QMP `cont`). The names match the TS
/// `InstanceState` union exactly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstanceState {
    /// No Instance exists.
    None,
    /// A create is in flight (the single slot is reserved).
    Starting,
    /// An Instance is running with its Guest CPUs live.
    Running,
    /// An Instance is running with its Guest CPUs paused (QMP `stop`).
    Paused,
    /// An Instance is being torn down (its slot is being released).
    Stopped,
}

impl InstanceState {
    /// The canonical UPPERCASE spelling, matching the TS state union and used in
    /// tool responses and actionable messages.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "NONE",
            Self::Starting => "STARTING",
            Self::Running => "RUNNING",
            Self::Paused => "PAUSED",
            Self::Stopped => "STOPPED",
        }
    }
}

impl std::fmt::Display for InstanceState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A read-only view of the current Instance for the tools to return.
#[derive(Debug, Clone)]
pub struct InstanceView {
    /// Current lifecycle state.
    pub state: InstanceState,
    /// The validated Hardware Spec, when an Instance exists.
    pub spec: Option<HardwareSpec>,
    /// The accelerator the running Instance was launched with.
    pub accel: Option<Accel>,
}

/// The result of a successful [`Orchestrator::create_instance`].
#[derive(Debug, Clone)]
pub struct CreateInstanceResult {
    /// Always [`InstanceState::Running`] on success.
    pub state: InstanceState,
    /// The validated Hardware Spec the Instance was built from.
    pub spec: HardwareSpec,
    /// The accelerator actually chosen (KVM or TCG).
    pub accel: Accel,
    /// Why that accelerator was chosen — reported to the agent (ADR-0008).
    pub accel_reason: String,
}

/// Knobs the Orchestrator needs that are not part of the Hardware Spec. The
/// singleton injects the env-resolved values (mirrors the TS `OrchestratorOptions`,
/// trimmed to what this slice's `build_argv` + accel resolution consume).
pub struct OrchestratorOptions {
    /// The `qemu-system-*` binary to launch.
    pub binary: String,
    /// Server-managed path of the QMP UNIX socket.
    pub qmp_socket_path: String,
    /// Absolute path of the Image Store directory (ADR-0006); required by a spec
    /// with disks.
    pub image_dir: Option<String>,
    /// Absolute path of the read-only ISO Store directory (ADR-0006); required by a
    /// spec with a cdrom.
    pub iso_dir: Option<String>,
    /// Inclusive host-port range a user-mode forward's `hostPort` must fall within
    /// (ADR-0009); `None` uses the argv builder's default.
    pub hostfwd_port_range: Option<PortRange>,
    /// Whether host-level (`tap`/`bridge`) networking is permitted (ADR-0009).
    pub allow_host_net: bool,
    /// Hard cap, in MiB, on the spec's `memoryMb` (issue #9); `None` skips it.
    pub max_memory_mb: Option<u32>,
    /// Hard cap on the spec's `vcpus` (issue #9); `None` skips it.
    pub max_vcpus: Option<u32>,
    /// Whether the raw-args escape hatch is enabled (`QMP_MCP_ALLOW_RAW_ARGS`,
    /// ADR-0002).
    pub allow_raw_args: bool,
    /// Probe for KVM availability (injected for testability; production passes the
    /// `/dev/kvm` probe, tests force a deterministic value).
    pub kvm_available: Box<dyn Fn() -> bool + Send + Sync>,
}

/// Raised for lifecycle violations (creating while an Instance exists, destroying
/// when none does, driving a non-existent Instance) and for any create-time
/// validation/launch failure. The message is always actionable — it names the cause
/// and the remediation — mirroring the TS `LifecycleError`.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct LifecycleError(pub String);

/// Holds the single managed Instance: exactly one exists at a time. Requesting a
/// new Instance while one exists is rejected rather than auto-replaced (ADR-0004).
/// Not `Clone` and not thread-safe on its own — it is shared as an
/// `Arc<Mutex<Orchestrator>>`, which is what serialises concurrent tool calls.
pub struct Orchestrator {
    driver: Box<dyn QemuDriver>,
    options: OrchestratorOptions,
    state: InstanceState,
    handle: Option<Box<dyn InstanceHandle>>,
    spec: Option<HardwareSpec>,
    accel: Option<Accel>,
}

impl Orchestrator {
    /// Construct an Orchestrator over an injected [`QemuDriver`]. Starts in
    /// [`InstanceState::None`] with no Instance.
    pub fn new(driver: Box<dyn QemuDriver>, options: OrchestratorOptions) -> Self {
        Self {
            driver,
            options,
            state: InstanceState::None,
            handle: None,
            spec: None,
            accel: None,
        }
    }

    /// The current lifecycle state (a cheap accessor for the shutdown hook and
    /// tests, avoiding an [`InstanceView`] clone).
    pub fn state(&self) -> InstanceState {
        self.state
    }

    /// Return the current Instance view. Reports `NONE` when nothing is running.
    pub fn get_instance(&self) -> InstanceView {
        InstanceView {
            state: self.state,
            spec: self.spec.clone(),
            accel: self.accel,
        }
    }

    /// Build and launch a new Instance from an untrusted candidate Hardware Spec,
    /// negotiate its QMP Session (owned by the returned handle), and bring it to
    /// `RUNNING`. Rejects with an actionable message when an Instance already
    /// exists. A validation or launch failure releases the reserved slot back to
    /// `NONE`, so a later create can proceed.
    pub async fn create_instance(
        &mut self,
        candidate: serde_json::Value,
    ) -> Result<CreateInstanceResult, LifecycleError> {
        if self.state != InstanceState::None {
            return Err(LifecycleError(format!(
                "An Instance already exists (state {}). Only one Instance may run at a time — \
                 destroy it with destroy_instance before creating a new one.",
                self.state
            )));
        }

        // Reserve the single slot. Because the whole Orchestrator lives behind an
        // `Arc<Mutex>`, this method runs to completion under the caller's lock, so
        // no concurrent create can observe STARTING and race us — the create-time
        // TOCTOU is structurally impossible (ADR-0011).
        self.state = InstanceState::Starting;
        match self.launch_instance(candidate).await {
            Ok((spec, resolution, handle)) => {
                self.handle = Some(handle);
                self.spec = Some(spec.clone());
                self.accel = Some(resolution.accel);
                self.state = InstanceState::Running;
                tracing::info!("Instance RUNNING ({})", resolution.reason);
                Ok(CreateInstanceResult {
                    state: InstanceState::Running,
                    spec,
                    accel: resolution.accel,
                    accel_reason: resolution.reason,
                })
            }
            Err(err) => {
                // Validation or launch failed: free the reserved slot (nothing was
                // published, so there is no handle to tear down).
                self.state = InstanceState::None;
                Err(err)
            }
        }
    }

    /// The fallible half of [`create_instance`](Self::create_instance): validate the
    /// candidate spec (slice-2 parser), resolve the accelerator, generate the argv,
    /// and launch via the driver. Borrows `&self` only, so it commits nothing — the
    /// caller publishes the Instance (or releases the slot) based on the outcome.
    async fn launch_instance(
        &self,
        candidate: serde_json::Value,
    ) -> Result<(HardwareSpec, AccelResolution, Box<dyn InstanceHandle>), LifecycleError> {
        let spec = parse_hardware_spec(candidate).map_err(|e| LifecycleError(e.0))?;
        let resolution = resolve_accel(spec.accel, || (self.options.kvm_available)())
            .map_err(|e| LifecycleError(e.0))?;
        let argv = build_argv(&spec, &self.argv_options(resolution.accel))
            .map_err(|e| LifecycleError(e.0))?;
        tracing::info!(
            "creating Instance (machine={}, accel={})",
            spec.machine.as_str(),
            resolution.accel.as_str()
        );
        let handle = self
            .driver
            .launch(LaunchRequest {
                binary: self.options.binary.clone(),
                argv,
                qmp_socket_path: self.options.qmp_socket_path.clone(),
            })
            .await
            .map_err(|e| LifecycleError(format!("Failed to create the Instance: {}", e.0)))?;
        Ok((spec, resolution, handle))
    }

    /// Assemble the [`ArgvOptions`] the pure argv generator needs from this slice's
    /// options plus the resolved accelerator.
    fn argv_options(&self, accel: Accel) -> ArgvOptions {
        ArgvOptions {
            accel,
            qmp_socket_path: self.options.qmp_socket_path.clone(),
            image_dir: self.options.image_dir.clone(),
            iso_dir: self.options.iso_dir.clone(),
            hostfwd_port_range: self.options.hostfwd_port_range,
            allow_host_net: self.options.allow_host_net,
            max_memory_mb: self.options.max_memory_mb,
            max_vcpus: self.options.max_vcpus,
            allow_raw_args: self.options.allow_raw_args,
        }
    }

    /// Terminate the running Instance's process, close its QMP Session, and return
    /// to `NONE`. Rejects when no Instance exists. State returns to `NONE` even if
    /// the driver's `close` reports an error (mirrors the TS `finally`).
    pub async fn destroy_instance(&mut self) -> Result<(), LifecycleError> {
        if self.state == InstanceState::None || self.handle.is_none() {
            return Err(LifecycleError(
                "No Instance is running, so there is nothing to destroy. Create one with \
                 create_instance first."
                    .to_string(),
            ));
        }
        // Claim the teardown: take the handle and clear the Instance fields before
        // awaiting close. (The outer mutex already serialises callers, so this
        // cannot interleave with another destroy.)
        let handle = self
            .handle
            .take()
            .expect("handle present in a non-NONE state");
        self.spec = None;
        self.accel = None;
        self.state = InstanceState::Stopped;
        tracing::info!("destroying Instance");
        let closed = handle.close().await;
        self.state = InstanceState::None;
        closed.map_err(|e| LifecycleError(format!("Failed to destroy the Instance: {}", e.0)))?;
        tracing::info!("Instance destroyed (state NONE)");
        Ok(())
    }

    /// Return the live QMP `query-status` result for the running Instance (the run
    /// state of the Guest CPUs). With the fake driver this is the tracked run-state;
    /// with the real driver (slice #21) it is an actual QMP round-trip. Rejects when
    /// no Instance is running.
    pub async fn get_status(&self) -> Result<serde_json::Value, LifecycleError> {
        self.require_handle("query its status")?
            .execute("query-status", None)
            .await
            .map_err(|e| LifecycleError(e.0))
    }

    /// Pause the running Instance's Guest CPUs via QMP `stop`, moving the lifecycle
    /// RUNNING → PAUSED (reflected by `get_status`, which then reports `paused`).
    /// Rejects when no Instance is running.
    pub async fn pause_instance(&mut self) -> Result<InstanceState, LifecycleError> {
        self.require_handle("pause it")?
            .execute("stop", None)
            .await
            .map_err(|e| LifecycleError(e.0))?;
        self.state = InstanceState::Paused;
        tracing::info!("Instance PAUSED (QMP stop)");
        Ok(self.state)
    }

    /// Resume the Instance's Guest CPUs via QMP `cont`, moving the lifecycle
    /// PAUSED → RUNNING. Rejects when no Instance is running.
    pub async fn resume_instance(&mut self) -> Result<InstanceState, LifecycleError> {
        self.require_handle("resume it")?
            .execute("cont", None)
            .await
            .map_err(|e| LifecycleError(e.0))?;
        self.state = InstanceState::Running;
        tracing::info!("Instance RUNNING (QMP cont)");
        Ok(self.state)
    }

    /// Borrow the live [`InstanceHandle`] for an action that requires a running
    /// Instance, or return an actionable [`LifecycleError`] naming the action. The
    /// handle is only present in RUNNING/PAUSED, so this also fail-closes the
    /// STARTING/STOPPED/NONE cases.
    fn require_handle(&self, action: &str) -> Result<&dyn InstanceHandle, LifecycleError> {
        match &self.handle {
            Some(handle) => Ok(handle.as_ref()),
            None => Err(LifecycleError(format!(
                "No Instance is running, so there is nothing to {action}. Create one with \
                 create_instance first."
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use serde_json::json;
    use tokio::sync::Mutex as AsyncMutex;

    use super::*;
    use crate::qemu::driver::FakeQemuDriver;

    /// Deterministic options for the lifecycle tests: force TCG (no `/dev/kvm`
    /// probe), a fixed socket, no stores or caps. A diskless empty spec launches
    /// cleanly against these.
    fn test_options() -> OrchestratorOptions {
        OrchestratorOptions {
            binary: "qemu-system-x86_64".to_string(),
            qmp_socket_path: "/run/qmp-mcp/qmp.sock".to_string(),
            image_dir: None,
            iso_dir: None,
            hostfwd_port_range: None,
            allow_host_net: false,
            max_memory_mb: None,
            max_vcpus: None,
            allow_raw_args: false,
            kvm_available: Box::new(|| false),
        }
    }

    fn orchestrator_with(driver: FakeQemuDriver) -> Orchestrator {
        Orchestrator::new(Box::new(driver), test_options())
    }

    #[tokio::test]
    async fn create_brings_instance_to_running_then_destroy_returns_to_none() {
        let mut orch = orchestrator_with(FakeQemuDriver::new());
        assert_eq!(orch.state(), InstanceState::None);
        assert_eq!(orch.get_instance().state, InstanceState::None);

        let result = orch.create_instance(json!({})).await.expect("create ok");
        assert_eq!(result.state, InstanceState::Running);
        assert_eq!(result.accel, Accel::Tcg); // kvm_available == false → TCG

        let view = orch.get_instance();
        assert_eq!(view.state, InstanceState::Running);
        assert!(view.spec.is_some());
        assert_eq!(view.accel, Some(Accel::Tcg));

        orch.destroy_instance().await.expect("destroy ok");
        assert_eq!(orch.state(), InstanceState::None);
        assert!(orch.get_instance().spec.is_none());
        assert!(orch.get_instance().accel.is_none());
    }

    #[tokio::test]
    async fn build_argv_output_reaches_the_driver() {
        let driver = FakeQemuDriver::new();
        let launches = driver.launches();
        let mut orch = orchestrator_with(driver);
        orch.create_instance(json!({})).await.unwrap();

        let launches = launches.lock().unwrap();
        assert_eq!(launches.len(), 1);
        let argv = &launches[0].argv;
        // The generated argv is headless/frozen and wires the managed QMP socket.
        assert!(argv.iter().any(|a| a == "-qmp"));
        assert!(argv.iter().any(|a| a == "-S"));
        assert_eq!(launches[0].binary, "qemu-system-x86_64");
    }

    #[tokio::test]
    async fn create_while_running_is_rejected_with_actionable_message() {
        let mut orch = orchestrator_with(FakeQemuDriver::new());
        orch.create_instance(json!({})).await.unwrap();

        let err = orch.create_instance(json!({})).await.unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("An Instance already exists"), "got: {msg}");
        assert!(msg.contains("state RUNNING"), "got: {msg}");
        assert!(msg.contains("destroy_instance"), "got: {msg}");
        // Still exactly one Instance, still RUNNING.
        assert_eq!(orch.state(), InstanceState::Running);
    }

    #[tokio::test]
    async fn destroy_without_instance_is_rejected_actionably() {
        let mut orch = orchestrator_with(FakeQemuDriver::new());
        let err = orch.destroy_instance().await.unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("No Instance is running"), "got: {msg}");
        assert!(msg.contains("create_instance"), "got: {msg}");
    }

    #[tokio::test]
    async fn get_status_reflects_tracked_run_state_across_pause_resume() {
        let mut orch = orchestrator_with(FakeQemuDriver::new());
        // No Instance → get_status is refused (get_instance is the NONE-safe view).
        assert!(orch.get_status().await.is_err());

        orch.create_instance(json!({})).await.unwrap();
        let status = orch.get_status().await.unwrap();
        assert_eq!(status["status"], "running");
        assert_eq!(status["running"], true);

        // RUNNING → PAUSED (QMP stop), reflected by the live status.
        assert_eq!(orch.pause_instance().await.unwrap(), InstanceState::Paused);
        assert_eq!(orch.state(), InstanceState::Paused);
        let status = orch.get_status().await.unwrap();
        assert_eq!(status["status"], "paused");
        assert_eq!(status["running"], false);

        // PAUSED → RUNNING (QMP cont).
        assert_eq!(
            orch.resume_instance().await.unwrap(),
            InstanceState::Running
        );
        assert_eq!(orch.state(), InstanceState::Running);
        assert_eq!(orch.get_status().await.unwrap()["status"], "running");
    }

    #[tokio::test]
    async fn invalid_spec_is_rejected_before_any_launch() {
        let driver = FakeQemuDriver::new();
        let launches = driver.launches();
        let mut orch = orchestrator_with(driver);

        let err = orch
            .create_instance(json!({ "vcpus": 0 }))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("vcpus"), "got: {err}");
        assert_eq!(orch.state(), InstanceState::None); // slot released
        assert_eq!(
            launches.lock().unwrap().len(),
            0,
            "no launch on invalid spec"
        );
    }

    #[tokio::test]
    async fn launch_failure_releases_the_slot_and_reports_actionably() {
        let mut orch =
            orchestrator_with(FakeQemuDriver::with_launch_error("qemu binary not found"));
        let err = orch.create_instance(json!({})).await.unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("Failed to create the Instance"), "got: {msg}");
        assert!(msg.contains("qemu binary not found"), "got: {msg}");
        // The reserved slot is freed, so the Orchestrator is back to NONE.
        assert_eq!(orch.state(), InstanceState::None);
    }

    /// ADR-0011: concurrent `create_instance` calls serialise on the mutex and
    /// exactly one wins — the create-time TOCTOU is structurally impossible. Uses a
    /// multi-threaded runtime so the tasks genuinely contend for the lock.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_create_calls_serialize_and_exactly_one_wins() {
        let driver = FakeQemuDriver::new();
        let launches = driver.launches();
        let orch = Arc::new(AsyncMutex::new(orchestrator_with(driver)));

        let attempts = 16;
        let mut tasks = Vec::with_capacity(attempts);
        for _ in 0..attempts {
            let orch = Arc::clone(&orch);
            tasks.push(tokio::spawn(async move {
                // The caller holds the async mutex for the whole create — including
                // the launch await — so the calls fully serialise.
                let mut guard = orch.lock().await;
                guard.create_instance(json!({})).await
            }));
        }

        let mut wins = 0usize;
        let mut rejects = 0usize;
        for task in tasks {
            match task.await.expect("task joined") {
                Ok(result) => {
                    assert_eq!(result.state, InstanceState::Running);
                    wins += 1;
                }
                Err(err) => {
                    assert!(
                        err.to_string().contains("An Instance already exists"),
                        "unexpected error: {err}"
                    );
                    rejects += 1;
                }
            }
        }

        assert_eq!(wins, 1, "exactly one create must win");
        assert_eq!(rejects, attempts - 1);
        // The structural guarantee: the driver launched exactly once — no second
        // qemu was ever spawned and orphaned.
        assert_eq!(
            launches.lock().unwrap().len(),
            1,
            "the driver must have launched exactly once"
        );
        assert_eq!(orch.lock().await.state(), InstanceState::Running);
    }
}
